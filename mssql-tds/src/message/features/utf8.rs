// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;

use crate::core::TdsResult;
use crate::{
    io::packet_writer::{PacketWriter, TdsPacketWriter},
    message::login::{Feature, FeatureExtension},
};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Utf8Feature {
    acknowledged: bool,
}

#[async_trait]
impl Feature for Utf8Feature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::Utf8Support
    }

    fn is_requested(&self) -> bool {
        true
    }

    fn data_length(&self) -> i32 {
        (size_of::<u8>() + size_of::<i32>()) as i32
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;
        packet_writer.write_i32_async(0).await?;
        Ok(())
    }

    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()> {
        if data.len() != 1 {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid data length {} for UTF-8 feature, expected 1 byte",
                data.len()
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
    fn utf8_feature_defaults() {
        let f = Utf8Feature::default();
        assert!(!f.is_acknowledged());
        assert!(f.is_requested());
        assert_eq!(f.data_length(), (size_of::<u8>() + size_of::<i32>()) as i32);
    }

    #[test]
    fn utf8_feature_deserialize_valid() {
        let mut f = Utf8Feature::default();
        assert!(f.deserialize(&[0x01]).is_ok());
    }

    #[test]
    fn utf8_feature_deserialize_invalid_length() {
        let mut f = Utf8Feature::default();
        assert!(f.deserialize(&[]).is_err());
        assert!(f.deserialize(&[0x01, 0x02]).is_err());
    }

    #[test]
    fn utf8_feature_acknowledged() {
        let mut f = Utf8Feature::default();
        assert!(!f.is_acknowledged());
        f.set_acknowledged(true);
        assert!(f.is_acknowledged());
    }

    #[test]
    fn utf8_feature_identifier() {
        let f = Utf8Feature::default();
        assert_eq!(f.feature_identifier(), FeatureExtension::Utf8Support);
    }

    #[test]
    fn utf8_feature_clone_box() {
        let f = Utf8Feature::default();
        let cloned = f.clone_box();
        assert_eq!(cloned.feature_identifier(), FeatureExtension::Utf8Support);
    }
}
