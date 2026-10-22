// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # Enhanced Routing Feature Extension
//!
//! LOGIN7 feature extension `0x0F` (ENHANCEDROUTINGSUPPORT) carries no data. It
//! tells the server that the client can act on ENVCHANGE type 21, which routes
//! the connection to a database on an alternate server ([MS-TDS] 2.2.6.4,
//! 2.2.7.9).
//!
//! The acknowledgement is a single `IsEnabled` byte. msodbcsql treats any other
//! length or value as a protocol error (`Parse.cpp`, `FEACK_ENHANCEDROUTING`).

use async_trait::async_trait;

use crate::core::TdsResult;
use crate::error::Error;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::login::{Feature, FeatureExtension};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EnhancedRoutingFeature {
    acknowledged: bool,
    enabled: bool,
}

#[async_trait]
impl Feature for EnhancedRoutingFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::EnhancedRouting
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
        let &[is_enabled @ (0 | 1)] = data else {
            return Err(Error::ProtocolError(format!(
                "Invalid Enhanced Routing feature acknowledgement {data:02X?}, \
                 expected a single byte of 0 or 1"
            )));
        };
        self.enabled = is_enabled == 1;
        Ok(())
    }

    fn enhanced_routing_enabled(&self) -> bool {
        self.acknowledged && self.enabled
    }

    fn is_acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn set_acknowledged(&mut self, acknowledged: bool) {
        self.acknowledged = acknowledged;
        if !acknowledged {
            self.enabled = false;
        }
    }

    fn clone_box(&self) -> Box<dyn Feature> {
        Box::new(*self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use crate::message::messages::PacketType;
    use futures::executor::block_on;

    fn acknowledged(payload: &[u8]) -> TdsResult<EnhancedRoutingFeature> {
        let mut feature = EnhancedRoutingFeature::default();
        feature.set_acknowledged(true);
        feature.deserialize(payload)?;
        Ok(feature)
    }

    #[test]
    fn identifies_as_feature_0x0f() {
        let feature = EnhancedRoutingFeature::default();
        assert_eq!(
            feature.feature_identifier(),
            FeatureExtension::EnhancedRouting
        );
        assert_eq!(feature.feature_identifier().as_u8(), 0x0F);
        assert!(feature.is_requested());
    }

    #[test]
    fn serializes_without_feature_data() {
        let feature = EnhancedRoutingFeature::default();
        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut packet_writer = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(feature.serialize(&mut packet_writer)).unwrap();
        let bytes =
            packet_writer.get_payload().get_ref()[PacketWriter::PACKET_HEADER_SIZE..].to_vec();
        assert_eq!(bytes, [0x0F, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(bytes.len(), feature.data_length() as usize);
    }

    #[test]
    fn acknowledgement_of_one_enables_routing() {
        assert!(acknowledged(&[0x01]).unwrap().enhanced_routing_enabled());
    }

    #[test]
    fn acknowledgement_of_zero_leaves_routing_disabled() {
        let feature = acknowledged(&[0x00]).unwrap();
        assert!(feature.is_acknowledged());
        assert!(!feature.enhanced_routing_enabled());
    }

    #[test]
    fn unacknowledged_feature_is_disabled() {
        assert!(!EnhancedRoutingFeature::default().enhanced_routing_enabled());
    }

    #[test]
    fn rejects_malformed_acknowledgements() {
        for payload in [&[][..], &[0x02], &[0xFF], &[0x01, 0x00]] {
            let error = acknowledged(payload).unwrap_err();
            assert!(
                matches!(error, Error::ProtocolError(_)),
                "{payload:?} should be a protocol error, got {error:?}"
            );
        }
    }

    #[test]
    fn clearing_the_acknowledgement_disables_routing() {
        let mut feature = acknowledged(&[0x01]).unwrap();
        feature.set_acknowledged(false);
        assert!(!feature.enhanced_routing_enabled());
    }

    #[test]
    fn clone_box_keeps_the_negotiated_state() {
        let feature = acknowledged(&[0x01]).unwrap();
        let cloned = feature.clone_box();
        assert_eq!(
            cloned.feature_identifier(),
            FeatureExtension::EnhancedRouting
        );
        assert!(cloned.enhanced_routing_enabled());
    }
}
