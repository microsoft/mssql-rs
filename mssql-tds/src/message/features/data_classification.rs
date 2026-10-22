// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # Data Classification Feature Extension
//!
//! Implements the LOGIN7 feature extension `0x09` (`FEATUREEXT_DATACLASSIFICATION`).
//! The feature-specific data is a single byte carrying the *requested*
//! `DATACLASSIFICATION_VERSION`.
//!
//! The server answers in FEATUREEXTACK with
//! `DATACLASSIFICATION_VERSION (BYTE) | IsEnabled (BYTE) | VersionSpecificData`.
//! The requested version is the client's maximum, so the server normally
//! acknowledges it or a lower one. Any supported version is accepted, as in
//! msodbcsql, SqlClient and JDBC, which bound the acknowledgement by their
//! highest supported version rather than the request. The `0xA3` token layout
//! is the lower of the requested and acknowledged versions: [MS-TDS] 2.2.7.5
//! sends `SensitivityRank` only when both are version 2. msodbcsql takes the
//! layout from the acknowledgement alone. The version applies only when
//! `IsEnabled` is non-zero.
//!
//! `IsEnabled == 0` leaves the feature off, as in SqlClient and JDBC.
//! msodbcsql requires the byte but never reads it, so it enables the feature
//! regardless.

use async_trait::async_trait;
use tracing::warn;

use crate::core::TdsResult;
use crate::error::Error;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::login::{Feature, FeatureExtension};
use crate::token::data_classification::DataClassificationVersion;

/// Bytes before the optional version-specific data in the FEATUREEXTACK payload.
const ACK_FIXED_LENGTH: usize = 2;

/// LOGIN7 request for Data Classification metadata at a given version.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DataClassificationFeature {
    acknowledged: bool,
    requested_version: DataClassificationVersion,
    /// Version the server enabled; `None` until acknowledged, or when the
    /// server acknowledged with `IsEnabled = 0`.
    negotiated_version: Option<DataClassificationVersion>,
}

impl DataClassificationFeature {
    pub(crate) fn new(requested_version: DataClassificationVersion) -> Self {
        Self {
            acknowledged: false,
            requested_version,
            negotiated_version: None,
        }
    }
}

#[async_trait]
impl Feature for DataClassificationFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::DataClassification
    }

    fn is_requested(&self) -> bool {
        // Registered only when the connection requests a version.
        true
    }

    fn data_length(&self) -> i32 {
        // 1 byte feature identifier + 4 bytes length + 1 byte version.
        (size_of::<u8>() + size_of::<i32>() + size_of::<u8>()) as i32
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;
        packet_writer.write_i32_async(1).await?;
        packet_writer
            .write_byte_async(self.requested_version as u8)
            .await?;
        Ok(())
    }

    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()> {
        let [version, is_enabled, ..] = *data else {
            return Err(Error::ProtocolError(format!(
                "Invalid data length {} for Data Classification feature acknowledgement, \
                 expected at least {ACK_FIXED_LENGTH} bytes",
                data.len()
            )));
        };
        let version = DataClassificationVersion::try_from(version).map_err(|_| {
            Error::ProtocolError(format!(
                "Invalid Data Classification feature acknowledgement version {version}"
            ))
        })?;
        let layout = if (version as u8) < (self.requested_version as u8) {
            warn!(
                requested = ?self.requested_version,
                acknowledged = ?version,
                "Server acknowledged a lower Data Classification version than requested"
            );
            version
        } else {
            self.requested_version
        };
        self.negotiated_version = (is_enabled != 0).then_some(layout);
        Ok(())
    }

    fn negotiated_data_classification_version(&self) -> Option<DataClassificationVersion> {
        self.acknowledged
            .then_some(self.negotiated_version)
            .flatten()
    }

    fn is_acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn set_acknowledged(&mut self, acknowledged: bool) {
        self.acknowledged = acknowledged;
        if !acknowledged {
            self.negotiated_version = None;
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
    use crate::io::token_stream::{GenericTokenParserRegistry, TokenParserRegistry};
    use crate::message::messages::PacketType;
    use crate::token::tokens::TokenType;
    use futures::executor::block_on;

    fn serialized(version: DataClassificationVersion) -> Vec<u8> {
        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut packet_writer = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);
        block_on(DataClassificationFeature::new(version).serialize(&mut packet_writer)).unwrap();
        packet_writer.get_payload().get_ref()[PacketWriter::PACKET_HEADER_SIZE..].to_vec()
    }

    #[test]
    fn identifies_as_feature_0x09() {
        let feature = DataClassificationFeature::new(DataClassificationVersion::Version1);
        assert_eq!(
            feature.feature_identifier(),
            FeatureExtension::DataClassification
        );
        assert_eq!(feature.feature_identifier().as_u8(), 0x09);
        assert!(feature.is_requested());
    }

    #[test]
    fn data_length_covers_id_length_and_one_version_byte() {
        let feature = DataClassificationFeature::new(DataClassificationVersion::Version2);
        assert_eq!(feature.data_length(), 6);
    }

    #[test]
    fn serializes_version1_request() {
        assert_eq!(
            serialized(DataClassificationVersion::Version1),
            [0x09, 0x01, 0x00, 0x00, 0x00, 0x01]
        );
    }

    #[test]
    fn serializes_version2_request() {
        assert_eq!(
            serialized(DataClassificationVersion::Version2),
            [0x09, 0x01, 0x00, 0x00, 0x00, 0x02]
        );
    }

    #[test]
    fn serialized_size_matches_data_length() {
        for version in [
            DataClassificationVersion::Version1,
            DataClassificationVersion::Version2,
        ] {
            let feature = DataClassificationFeature::new(version);
            assert_eq!(serialized(version).len(), feature.data_length() as usize);
        }
    }

    fn acknowledged(
        requested: DataClassificationVersion,
        payload: &[u8],
    ) -> TdsResult<DataClassificationFeature> {
        let mut feature = DataClassificationFeature::new(requested);
        feature.set_acknowledged(true);
        feature.deserialize(payload)?;
        Ok(feature)
    }

    #[test]
    fn unacknowledged_feature_has_no_negotiated_version() {
        let feature = DataClassificationFeature::new(DataClassificationVersion::Version2);
        assert!(!feature.is_acknowledged());
        assert_eq!(feature.negotiated_data_classification_version(), None);
    }

    #[test]
    fn enabled_acknowledgement_negotiates_the_acknowledged_version() {
        for (requested, ack_version, expected) in [
            (
                DataClassificationVersion::Version1,
                0x01,
                DataClassificationVersion::Version1,
            ),
            (
                DataClassificationVersion::Version2,
                0x02,
                DataClassificationVersion::Version2,
            ),
            // Requested V2, server only supports V1: the token layout follows the server.
            (
                DataClassificationVersion::Version2,
                0x01,
                DataClassificationVersion::Version1,
            ),
        ] {
            let feature = acknowledged(requested, &[ack_version, 0x01]).unwrap();
            assert_eq!(
                feature.negotiated_data_classification_version(),
                Some(expected)
            );
        }
    }

    /// msodbcsql (`Parse.cpp`), SqlClient and JDBC bound the acknowledgement
    /// by the highest version they support, so a higher one is accepted. The
    /// layout stays V1: [MS-TDS] 2.2.7.5 sends ranks only when both are V2.
    #[test]
    fn accepts_a_higher_acknowledgement_with_the_requested_layout() {
        let feature = acknowledged(DataClassificationVersion::Version1, &[0x02, 0x01]).unwrap();
        assert_eq!(
            feature.negotiated_data_classification_version(),
            Some(DataClassificationVersion::Version1)
        );
    }

    #[test]
    fn disabled_acknowledgement_negotiates_nothing() {
        let feature = acknowledged(DataClassificationVersion::Version2, &[0x02, 0x00]).unwrap();
        assert!(feature.is_acknowledged());
        assert_eq!(feature.negotiated_data_classification_version(), None);
    }

    #[test]
    fn any_non_zero_is_enabled_byte_enables_the_feature() {
        let feature = acknowledged(DataClassificationVersion::Version2, &[0x02, 0xFF]).unwrap();
        assert_eq!(
            feature.negotiated_data_classification_version(),
            Some(DataClassificationVersion::Version2)
        );
    }

    #[test]
    fn version_specific_data_is_ignored() {
        let feature = acknowledged(
            DataClassificationVersion::Version2,
            &[0x02, 0x01, 0xAA, 0xBB],
        )
        .unwrap();
        assert_eq!(
            feature.negotiated_data_classification_version(),
            Some(DataClassificationVersion::Version2)
        );
    }

    #[test]
    fn rejects_short_acknowledgement() {
        for payload in [&[][..], &[0x02][..]] {
            let error = acknowledged(DataClassificationVersion::Version2, payload).unwrap_err();
            assert!(
                matches!(&error, Error::ProtocolError(message)
                    if message.contains(&format!("Invalid data length {}", payload.len()))),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_invalid_acknowledged_version() {
        for version in [0x00, 0x03, 0xFF] {
            let error =
                acknowledged(DataClassificationVersion::Version2, &[version, 0x01]).unwrap_err();
            assert!(
                matches!(&error, Error::ProtocolError(message)
                    if message.contains(&format!("acknowledgement version {version}"))),
                "{error}"
            );
        }
    }

    #[test]
    fn clearing_acknowledgement_drops_negotiated_version() {
        let mut feature = acknowledged(DataClassificationVersion::Version2, &[0x02, 0x01]).unwrap();
        feature.set_acknowledged(false);
        assert_eq!(feature.negotiated_data_classification_version(), None);
    }

    #[test]
    fn clone_box_preserves_requested_version() {
        let feature = DataClassificationFeature::new(DataClassificationVersion::Version1);
        let cloned = feature.clone_box();
        assert_eq!(
            cloned.feature_identifier(),
            FeatureExtension::DataClassification
        );
        assert_eq!(cloned.data_length(), feature.data_length());
    }

    #[test]
    fn clone_box_preserves_negotiated_version() {
        let feature = acknowledged(DataClassificationVersion::Version2, &[0x01, 0x01]).unwrap();
        assert_eq!(
            feature.clone_box().negotiated_data_classification_version(),
            Some(DataClassificationVersion::Version1)
        );
    }

    #[test]
    fn advertised_feature_has_a_registered_0xa3_parser() {
        let token_type = TokenType::try_from(0xA3).unwrap();
        assert_eq!(token_type, TokenType::DataClassification);
        assert!(
            GenericTokenParserRegistry::default()
                .get_parser(&token_type)
                .is_some()
        );
    }
}
