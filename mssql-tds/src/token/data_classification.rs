// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol models for TDS `DATACLASSIFICATION` metadata.

use crate::error::Error;

/// Supported `DATACLASSIFICATION` feature versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum DataClassificationVersion {
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
pub(crate) struct SensitivityLabel {
    /// Human-readable label name.
    pub(crate) name: String,
    /// Identifier used to link the label to an information protection system.
    pub(crate) id: String,
}

/// An information type from the token-level information-type dictionary.
///
/// An information type describes what the data represents, such as a national
/// identifier. It is independent of both the SQL data type and sensitivity label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InformationType {
    /// Human-readable information-type name.
    pub(crate) name: String,
    /// Identifier used to link the type to an information protection system.
    pub(crate) id: String,
}

/// A validated reference into the sensitivity-label dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SensitivityLabelReference {
    /// The wire index was `0xFFFF`, so no label is associated with the property.
    Unspecified,
    /// Zero-based index into [`DataClassificationMetadata::sensitivity_labels`].
    Index(u16),
}

/// A validated reference into the information-type dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InformationTypeReference {
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
pub(crate) enum SensitivityRank {
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
pub(crate) struct SensitivityProperty {
    /// Validated label-dictionary reference.
    pub(crate) sensitivity_label: SensitivityLabelReference,
    /// Validated information-type-dictionary reference.
    pub(crate) information_type: InformationTypeReference,
    /// Present only for version 2 metadata.
    pub(crate) rank: Option<SensitivityRank>,
}

/// Classification metadata for one result-set column.
///
/// An empty `properties` collection represents an unclassified column. A
/// derived column can contain multiple properties when it has multiple
/// classified source expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ColumnSensitivity {
    /// Classifications contributed by the column's source expressions.
    pub(crate) properties: Vec<SensitivityProperty>,
}

impl ColumnSensitivity {
    // Consumed by result-set integration; until then only tests call it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_classified(&self) -> bool {
        !self.properties.is_empty()
    }
}

/// Parsed TDS `DATACLASSIFICATION` metadata for a result set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DataClassificationMetadata {
    /// Negotiated version used to parse the token.
    pub(crate) version: DataClassificationVersion,
    /// Token-level sensitivity-label dictionary.
    pub(crate) sensitivity_labels: Vec<SensitivityLabel>,
    /// Token-level information-type dictionary.
    pub(crate) information_types: Vec<InformationType>,
    /// Overall result-set rank, present only for version 2.
    pub(crate) rank: Option<SensitivityRank>,
    /// Entries are in result-set column order.
    pub(crate) columns: Vec<ColumnSensitivity>,
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
}
