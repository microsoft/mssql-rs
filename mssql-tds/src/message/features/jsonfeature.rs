// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;

use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};

use crate::message::login::{Feature, FeatureExtension};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct JsonFeature {
    acknowledged: bool,
}

impl JsonFeature {
    pub const VERSION: u8 = 1;
}

#[async_trait]
impl Feature for JsonFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::Json
    }

    fn is_requested(&self) -> bool {
        true
    }

    fn data_length(&self) -> i32 {
        // 1 byte for feature identifier, 4 bytes for length, 1 byte for version
        (size_of::<u8>() + size_of::<i32>() + size_of::<u8>()) as i32
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;
        packet_writer.write_i32_async(1).await?;
        packet_writer.write_byte_async(Self::VERSION).await?;
        Ok(())
    }

    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()> {
        if data.len() != 1 {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid data length {} for JSON feature, expected 1 byte",
                data.len()
            )));
        }
        let server_supported_version = data[0];

        // Validate that the server supports the expected version or is 0 (indicating no support)
        if server_supported_version != Self::VERSION && server_supported_version != 0 {
            return Err(crate::error::Error::ProtocolError(format!(
                "Unsupported JSON feature version: {}, expected {} or 0",
                server_supported_version,
                Self::VERSION
            )));
        }
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
        let feature = JsonFeature::default();
        assert_eq!(feature.feature_identifier(), FeatureExtension::Json);
    }

    #[test]
    fn test_is_requested() {
        let feature = JsonFeature::default();
        assert!(feature.is_requested());
    }

    #[test]
    fn test_data_length() {
        let feature = JsonFeature::default();
        // 1 (feature id) + 4 (length) + 1 (version)
        assert_eq!(feature.data_length(), 6);
    }

    #[test]
    fn test_acknowledged() {
        let mut feature = JsonFeature::default();
        assert!(!feature.is_acknowledged());
        feature.set_acknowledged(true);
        assert!(feature.is_acknowledged());
    }

    #[test]
    fn test_deserialize_valid_version() {
        let mut feature = JsonFeature::default();
        feature.deserialize(&[JsonFeature::VERSION]).unwrap();
    }

    #[test]
    fn test_deserialize_version_zero() {
        let mut feature = JsonFeature::default();
        feature.deserialize(&[0u8]).unwrap();
    }

    #[test]
    fn test_deserialize_unsupported_version() {
        let mut feature = JsonFeature::default();
        let result = feature.deserialize(&[2u8]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unsupported JSON feature version")
        );
    }

    #[test]
    fn test_deserialize_invalid_length() {
        let mut feature = JsonFeature::default();
        let result = feature.deserialize(&[1u8, 0u8]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid data length")
        );
    }

    #[test]
    fn test_deserialize_empty() {
        let mut feature = JsonFeature::default();
        let result = feature.deserialize(&[]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid data length")
        );
    }

    #[test]
    fn test_clone_box() {
        let mut feature = JsonFeature::default();
        feature.set_acknowledged(true);
        let cloned = feature.clone_box();
        assert!(cloned.is_acknowledged());
        assert_eq!(cloned.feature_identifier(), FeatureExtension::Json);
    }
}
