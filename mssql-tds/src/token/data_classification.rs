// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Validated TDS `DATACLASSIFICATION` metadata for a result set.
//!
//! Request the feature with
//! [`ClientContext::requested_data_classification_version`](crate::connection::client_context::ClientContext::requested_data_classification_version)
//! and read a result set's classification through
//! [`ResultSet::data_classification`](crate::connection::tds_client::ResultSet::data_classification).
//! Values are only produced by the token parser, which has already checked
//! every count, dictionary reference, and rank.

use crate::error::Error;

/// Supported `DATACLASSIFICATION` feature versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum DataClassificationVersion {
    /// Base labels, information types, and per-column properties.
    Version1 = 1,
    /// Version 1 metadata plus result-set and per-property sensitivity ranks.
    Version2 = 2,
}

impl TryFrom<u8> for DataClassificationVersion {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Version1),
            2 => Ok(Self::Version2),
            _ => Err(Error::ProtocolError(format!(
                "DATACLASSIFICATION: unsupported version {value}; supported versions are 1 and 2"
            ))),
        }
    }
}

/// A sensitivity label from the token-level label dictionary.
///
/// A label describes the handling category of data, such as "Confidential".
/// It is independent of the column's SQL data type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SensitivityLabel {
    pub(crate) name: String,
    pub(crate) id: String,
}

impl SensitivityLabel {
    /// Human-readable label name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Identifier used to link the label to an information protection system.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// An information type from the token-level information-type dictionary.
///
/// An information type describes what the data represents, such as a national
/// identifier. It is independent of both the SQL data type and sensitivity label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InformationType {
    pub(crate) name: String,
    pub(crate) id: String,
}

impl InformationType {
    /// Human-readable information-type name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Identifier used to link the type to an information protection system.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// A validated reference into the sensitivity-label dictionary.
///
/// Resolve it with [`DataClassificationMetadata::resolve_label`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensitivityLabelReference {
    /// The wire index was `0xFFFF`, so no label is associated with the property.
    Unspecified,
    /// Zero-based index into [`DataClassificationMetadata::sensitivity_labels`].
    Index(u16),
}

/// A validated reference into the information-type dictionary.
///
/// Resolve it with [`DataClassificationMetadata::resolve_information_type`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InformationTypeReference {
    /// The wire index was `0xFFFF`, so no information type is associated with the property.
    Unspecified,
    /// Zero-based index into [`DataClassificationMetadata::information_types`].
    Index(u16),
}

/// Protocol-defined relative sensitivity rank.
///
/// Rank is an ordered sensitivity level. It is not a SQL data type, label, or
/// information type, and is present only in version 2 metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
#[non_exhaustive]
pub enum SensitivityRank {
    /// No rank was defined.
    NotDefined = -1,
    /// Explicitly non-sensitive.
    None = 0,
    /// Low sensitivity.
    Low = 10,
    /// Medium sensitivity.
    Medium = 20,
    /// High sensitivity.
    High = 30,
    /// Critical sensitivity.
    Critical = 40,
}

impl TryFrom<i32> for SensitivityRank {
    type Error = Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            -1 => Ok(Self::NotDefined),
            0 => Ok(Self::None),
            10 => Ok(Self::Low),
            20 => Ok(Self::Medium),
            30 => Ok(Self::High),
            40 => Ok(Self::Critical),
            _ => Err(Error::ProtocolError(format!(
                "DATACLASSIFICATION: unknown sensitivity rank {value}"
            ))),
        }
    }
}

/// Classification contributed by one source expression for a result column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SensitivityProperty {
    pub(crate) sensitivity_label: SensitivityLabelReference,
    pub(crate) information_type: InformationTypeReference,
    pub(crate) rank: Option<SensitivityRank>,
}

impl SensitivityProperty {
    /// Validated reference into the label dictionary.
    pub fn sensitivity_label(&self) -> SensitivityLabelReference {
        self.sensitivity_label
    }

    /// Validated reference into the information-type dictionary.
    pub fn information_type(&self) -> InformationTypeReference {
        self.information_type
    }

    /// Present only for version 2 metadata.
    pub fn rank(&self) -> Option<SensitivityRank> {
        self.rank
    }
}

/// Classification metadata for one result-set column.
///
/// An empty property list represents an unclassified column. A derived
/// column can contain multiple properties when it has multiple classified
/// source expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSensitivity {
    pub(crate) properties: Vec<SensitivityProperty>,
}

impl ColumnSensitivity {
    /// Classifications contributed by the column's source expressions.
    pub fn properties(&self) -> &[SensitivityProperty] {
        &self.properties
    }

    /// Returns whether the column has at least one sensitivity property.
    pub fn is_classified(&self) -> bool {
        !self.properties.is_empty()
    }
}

/// Parsed TDS `DATACLASSIFICATION` metadata for a result set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataClassificationMetadata {
    pub(crate) version: DataClassificationVersion,
    pub(crate) sensitivity_labels: Vec<SensitivityLabel>,
    pub(crate) information_types: Vec<InformationType>,
    pub(crate) rank: Option<SensitivityRank>,
    pub(crate) columns: Vec<ColumnSensitivity>,
}

impl DataClassificationMetadata {
    /// Negotiated version the token was parsed with.
    pub fn version(&self) -> DataClassificationVersion {
        self.version
    }

    /// Token-level sensitivity-label dictionary.
    pub fn sensitivity_labels(&self) -> &[SensitivityLabel] {
        &self.sensitivity_labels
    }

    /// Token-level information-type dictionary.
    pub fn information_types(&self) -> &[InformationType] {
        &self.information_types
    }

    /// Overall result-set rank, present only for version 2.
    pub fn rank(&self) -> Option<SensitivityRank> {
        self.rank
    }

    /// One entry per result-set column, in column order.
    pub fn columns(&self) -> &[ColumnSensitivity] {
        &self.columns
    }

    /// The label a property refers to, or `None` when it is unspecified.
    pub fn resolve_label(&self, reference: SensitivityLabelReference) -> Option<&SensitivityLabel> {
        match reference {
            SensitivityLabelReference::Unspecified => None,
            SensitivityLabelReference::Index(index) => {
                self.sensitivity_labels.get(usize::from(index))
            }
        }
    }

    /// The information type a property refers to, or `None` when it is unspecified.
    pub fn resolve_information_type(
        &self,
        reference: InformationTypeReference,
    ) -> Option<&InformationType> {
        match reference {
            InformationTypeReference::Unspecified => None,
            InformationTypeReference::Index(index) => {
                self.information_types.get(usize::from(index))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_typed_and_reject_unsupported_values() {
        assert_eq!(
            DataClassificationVersion::try_from(1).unwrap(),
            DataClassificationVersion::Version1
        );
        assert_eq!(
            DataClassificationVersion::try_from(2).unwrap(),
            DataClassificationVersion::Version2
        );

        let error = DataClassificationVersion::try_from(3).unwrap_err();
        assert!(error.to_string().contains("unsupported version 3"));
    }

    #[test]
    fn ranks_match_protocol_values_in_both_directions() {
        // [MS-TDS] DATACLASSIFICATION SensitivityRank table.
        let cases = [
            (-1, SensitivityRank::NotDefined),
            (0, SensitivityRank::None),
            (10, SensitivityRank::Low),
            (20, SensitivityRank::Medium),
            (30, SensitivityRank::High),
            (40, SensitivityRank::Critical),
        ];
        for (wire, rank) in cases {
            assert_eq!(SensitivityRank::try_from(wire).unwrap(), rank);
            assert_eq!(rank as i32, wire);
        }
    }

    #[test]
    fn ranks_reject_unknown_values() {
        for value in [-2, 1, 9, 11, 19, 21, 29, 31, 39, 41, 50, i32::MIN, i32::MAX] {
            let error = SensitivityRank::try_from(value).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("unknown sensitivity rank {value}")),
                "{error}"
            );
        }
    }

    #[test]
    fn references_resolve_to_their_dictionary_entries() {
        let metadata = DataClassificationMetadata {
            version: DataClassificationVersion::Version1,
            sensitivity_labels: vec![
                SensitivityLabel {
                    name: "Public".to_string(),
                    id: "L0".to_string(),
                },
                SensitivityLabel {
                    name: "Confidential".to_string(),
                    id: "L1".to_string(),
                },
            ],
            information_types: vec![InformationType {
                name: "National ID".to_string(),
                id: "I1".to_string(),
            }],
            rank: None,
            columns: Vec::new(),
        };

        let label = metadata
            .resolve_label(SensitivityLabelReference::Index(1))
            .unwrap();
        assert_eq!((label.name(), label.id()), ("Confidential", "L1"));
        let information_type = metadata
            .resolve_information_type(InformationTypeReference::Index(0))
            .unwrap();
        assert_eq!(
            (information_type.name(), information_type.id()),
            ("National ID", "I1")
        );
        assert_eq!(
            metadata.resolve_label(SensitivityLabelReference::Unspecified),
            None
        );
        assert_eq!(
            metadata.resolve_information_type(InformationTypeReference::Unspecified),
            None
        );
    }
}
