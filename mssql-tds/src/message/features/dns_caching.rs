// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # DNS Caching Feature Extension
//!
//! LOGIN7 feature extension `0x0B` (AZURESQLDNSCACHING) carries no data. Bit 0
//! of the acknowledgement tells the client whether it may keep the server's
//! host in its DNS cache ([MS-TDS] 2.2.7.11). msodbcsql reads the bit as
//! `len > 0 && (data[0] & 1)` and accepts any payload (`Parse.cpp`,
//! `FEACK_DNSCACHE`); so does this feature. See [`crate::connection::dns_cache`].

use async_trait::async_trait;

use crate::connection::dns_cache::Resolution;
use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::login::{Feature, FeatureExtension};

#[derive(Debug, Default, Clone)]
pub(crate) struct DnsCachingFeature {
    acknowledged: bool,
    allowed: bool,
    /// What the login's connection resolved; the acknowledgement applies to it.
    resolution: Option<Resolution>,
}

impl DnsCachingFeature {
    pub(crate) fn for_resolution(resolution: Resolution) -> Self {
        Self {
            resolution: Some(resolution),
            ..Self::default()
        }
    }
}

#[async_trait]
impl Feature for DnsCachingFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::SqlDnsCaching
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
        self.allowed = data.first().is_some_and(|flags| flags & 1 == 1);
        Ok(())
    }

    fn dns_caching_verdict(&self) -> Option<(&Resolution, bool)> {
        self.resolution
            .as_ref()
            .map(|resolution| (resolution, self.acknowledged && self.allowed))
    }

    fn is_acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn set_acknowledged(&mut self, acknowledged: bool) {
        self.acknowledged = acknowledged;
        if !acknowledged {
            self.allowed = false;
        }
    }

    fn clone_box(&self) -> Box<dyn Feature> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use crate::message::messages::PacketType;
    use futures::executor::block_on;

    fn resolution() -> Resolution {
        Resolution::new("db.example.com", vec!["10.0.0.7:1433".parse().unwrap()])
    }

    fn acknowledged(payload: &[u8]) -> DnsCachingFeature {
        let mut feature = DnsCachingFeature::for_resolution(resolution());
        feature.set_acknowledged(true);
        feature.deserialize(payload).unwrap();
        feature
    }

    #[test]
    fn identifies_as_feature_0x0b() {
        let feature = DnsCachingFeature::default();
        assert_eq!(
            feature.feature_identifier(),
            FeatureExtension::SqlDnsCaching
        );
        assert_eq!(feature.feature_identifier().as_u8(), 0x0B);
        assert!(feature.is_requested());
    }

    #[test]
    fn serializes_without_feature_data() {
        let feature = DnsCachingFeature::default();
        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut packet_writer = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(feature.serialize(&mut packet_writer)).unwrap();
        let bytes =
            packet_writer.get_payload().get_ref()[PacketWriter::PACKET_HEADER_SIZE..].to_vec();
        assert_eq!(bytes, [0x0B, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(bytes.len(), feature.data_length() as usize);
    }

    #[test]
    fn bit_zero_of_the_acknowledgement_allows_caching() {
        for (payload, allowed) in [
            (&[0x01][..], true),
            (&[0x03], true),
            (&[0x01, 0x00], true),
            (&[0x00], false),
            (&[0x02], false),
            (&[], false),
        ] {
            assert_eq!(
                acknowledged(payload).dns_caching_verdict(),
                Some((&resolution(), allowed)),
                "payload {payload:?}"
            );
        }
    }

    #[test]
    fn unacknowledged_feature_disallows_caching() {
        let feature = DnsCachingFeature::for_resolution(resolution());
        assert_eq!(feature.dns_caching_verdict(), Some((&resolution(), false)));
    }

    #[test]
    fn unbound_feature_has_no_verdict() {
        let mut feature = DnsCachingFeature::default();
        feature.set_acknowledged(true);
        feature.deserialize(&[0x01]).unwrap();
        assert_eq!(feature.dns_caching_verdict(), None);
    }

    #[test]
    fn clone_box_keeps_resolution_and_acknowledgement() {
        let cloned = acknowledged(&[0x01]).clone_box();
        assert_eq!(cloned.feature_identifier(), FeatureExtension::SqlDnsCaching);
        assert_eq!(cloned.dns_caching_verdict(), Some((&resolution(), true)));
    }
}
