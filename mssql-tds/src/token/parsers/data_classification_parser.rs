// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parser for the self-describing TDS `DATACLASSIFICATION` token payload.
//!
//! The token has no payload-length field. The dispatcher consumes token ID
//! `0xA3`, and this parser stops after the dictionary and per-column counts are
//! satisfied. Bytes that follow belong to the next token in the TDS stream.
//!
//! Reader errors pass through unchanged so callers can still tell a lost
//! transport from a malformed token. Only problems this parser detects itself
//! are reported as `DATACLASSIFICATION` protocol errors.
//!
//! The payload is validated while it is parsed: dictionary indexes must be in
//! range or `0xFFFF`, ranks must be defined values, and the token must fit the
//! parser's byte budget. SqlClient rejects out-of-range indexes and undefined
//! ranks the same way. msodbcsql validates nothing because it hands the
//! application raw bytes; this client exposes typed metadata, which cannot
//! represent an invalid reference.

use std::fmt;

use async_trait::async_trait;

use super::common::TokenParser;
use crate::core::TdsResult;
use crate::error::Error;
use crate::io::packet_reader::TdsPacketReader;
use crate::io::token_stream::ParserContext;
use crate::token::data_classification::{
    ColumnSensitivity, DataClassificationMetadata, DataClassificationVersion, InformationType,
    InformationTypeReference, SensitivityLabel, SensitivityLabelReference, SensitivityProperty,
    SensitivityRank,
};
use crate::token::tokens::Tokens;

const NULL_DICTIONARY_INDEX: u16 = u16::MAX;
const MAX_DATA_CLASSIFICATION_TOKEN_BYTES: usize = 256 * 1024;

#[derive(Debug, Default)]
pub(crate) struct DataClassificationTokenParser;

/// The wire field being read. Rendered only when an error is built, so a
/// successful parse allocates nothing for diagnostics.
#[derive(Clone, Copy)]
enum Field {
    LabelCount,
    LabelName(u16),
    LabelId(u16),
    InformationTypeCount,
    InformationTypeName(u16),
    InformationTypeId(u16),
    ResultSetRank,
    ColumnCount,
    PropertyCount { column: u16 },
    LabelIndex { column: u16, property: u16 },
    InformationTypeIndex { column: u16, property: u16 },
    PropertyRank { column: u16, property: u16 },
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Field::LabelCount => f.write_str("sensitivity-label count"),
            Field::LabelName(index) => write!(f, "sensitivity label {index} name"),
            Field::LabelId(index) => write!(f, "sensitivity label {index} id"),
            Field::InformationTypeCount => f.write_str("information-type count"),
            Field::InformationTypeName(index) => write!(f, "information type {index} name"),
            Field::InformationTypeId(index) => write!(f, "information type {index} id"),
            Field::ResultSetRank => f.write_str("result-set sensitivity rank"),
            Field::ColumnCount => f.write_str("result-set column count"),
            Field::PropertyCount { column } => {
                write!(f, "column {column} sensitivity-property count")
            }
            Field::LabelIndex { column, property } => {
                write!(
                    f,
                    "column {column} property {property} sensitivity-label index"
                )
            }
            Field::InformationTypeIndex { column, property } => {
                write!(
                    f,
                    "column {column} property {property} information-type index"
                )
            }
            Field::PropertyRank { column, property } => {
                write!(f, "column {column} property {property} sensitivity rank")
            }
        }
    }
}

#[derive(Default)]
struct ParseBudget {
    consumed: usize,
}

impl ParseBudget {
    fn claim(&mut self, bytes: usize, field: Field) -> TdsResult<()> {
        // Saturating: a total past the budget is rejected below either way.
        let consumed = self.consumed.saturating_add(bytes);
        if consumed > MAX_DATA_CLASSIFICATION_TOKEN_BYTES {
            return Err(classification_error(format!(
                "{field} exceeds the maximum supported token size of \
                 {MAX_DATA_CLASSIFICATION_TOKEN_BYTES} bytes"
            )));
        }
        self.consumed = consumed;
        Ok(())
    }

    fn validate_collection(
        &self,
        count: u16,
        minimum_item_bytes: usize,
        field: Field,
    ) -> TdsResult<()> {
        let minimum_bytes = usize::from(count).saturating_mul(minimum_item_bytes);
        let remaining = MAX_DATA_CLASSIFICATION_TOKEN_BYTES - self.consumed;
        if minimum_bytes > remaining {
            return Err(classification_error(format!(
                "{field} {count} requires at least {minimum_bytes} bytes, \
                 exceeding the remaining parser limit of {remaining} bytes"
            )));
        }
        Ok(())
    }
}

fn classification_error(message: impl Into<String>) -> Error {
    Error::ProtocolError(format!("DATACLASSIFICATION: {}", message.into()))
}

async fn read_u8<T: TdsPacketReader + Send + Sync>(
    reader: &mut T,
    budget: &mut ParseBudget,
    field: Field,
) -> TdsResult<u8> {
    budget.claim(1, field)?;
    reader.read_byte().await
}

async fn read_u16<T: TdsPacketReader + Send + Sync>(
    reader: &mut T,
    budget: &mut ParseBudget,
    field: Field,
) -> TdsResult<u16> {
    budget.claim(2, field)?;
    reader.read_uint16().await
}

async fn read_i32<T: TdsPacketReader + Send + Sync>(
    reader: &mut T,
    budget: &mut ParseBudget,
    field: Field,
) -> TdsResult<i32> {
    budget.claim(4, field)?;
    reader.read_int32().await
}

async fn read_rank<T: TdsPacketReader + Send + Sync>(
    reader: &mut T,
    budget: &mut ParseBudget,
    field: Field,
) -> TdsResult<SensitivityRank> {
    let value = read_i32(reader, budget, field).await?;
    SensitivityRank::try_from(value)
        .map_err(|_| classification_error(format!("{field}: unknown value {value}")))
}

async fn read_b_varchar<T: TdsPacketReader + Send + Sync>(
    reader: &mut T,
    budget: &mut ParseBudget,
    field: Field,
) -> TdsResult<String> {
    let character_count = usize::from(read_u8(reader, budget, field).await?);
    // At most 255 UTF-16 code units, so the byte count cannot overflow.
    budget.claim(character_count * 2, field)?;

    let mut units = Vec::with_capacity(character_count);
    for _ in 0..character_count {
        units.push(reader.read_uint16().await?);
    }
    String::from_utf16(&units)
        .map_err(|error| classification_error(format!("invalid UTF-16 in {field}: {error}")))
}

/// Validates a dictionary index; `None` is the `0xFFFF` "unspecified" value.
fn dictionary_index(index: u16, dictionary_len: usize, field: Field) -> TdsResult<Option<u16>> {
    if index == NULL_DICTIONARY_INDEX {
        return Ok(None);
    }
    if usize::from(index) >= dictionary_len {
        return Err(classification_error(format!(
            "{field} {index} is outside dictionary length {dictionary_len}"
        )));
    }
    Ok(Some(index))
}

fn label_reference(
    index: u16,
    dictionary_len: usize,
    field: Field,
) -> TdsResult<SensitivityLabelReference> {
    Ok(dictionary_index(index, dictionary_len, field)?.map_or(
        SensitivityLabelReference::Unspecified,
        SensitivityLabelReference::Index,
    ))
}

fn information_type_reference(
    index: u16,
    dictionary_len: usize,
    field: Field,
) -> TdsResult<InformationTypeReference> {
    Ok(dictionary_index(index, dictionary_len, field)?.map_or(
        InformationTypeReference::Unspecified,
        InformationTypeReference::Index,
    ))
}

impl DataClassificationTokenParser {
    pub(crate) async fn parse_with_version<T>(
        &self,
        reader: &mut T,
        version: DataClassificationVersion,
    ) -> TdsResult<DataClassificationMetadata>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let mut budget = ParseBudget::default();

        let label_count = read_u16(reader, &mut budget, Field::LabelCount).await?;
        budget.validate_collection(label_count, 2, Field::LabelCount)?;
        let mut sensitivity_labels = Vec::with_capacity(usize::from(label_count));
        for index in 0..label_count {
            sensitivity_labels.push(SensitivityLabel {
                name: read_b_varchar(reader, &mut budget, Field::LabelName(index)).await?,
                id: read_b_varchar(reader, &mut budget, Field::LabelId(index)).await?,
            });
        }

        let information_type_count =
            read_u16(reader, &mut budget, Field::InformationTypeCount).await?;
        budget.validate_collection(information_type_count, 2, Field::InformationTypeCount)?;
        let mut information_types = Vec::with_capacity(usize::from(information_type_count));
        for index in 0..information_type_count {
            information_types.push(InformationType {
                name: read_b_varchar(reader, &mut budget, Field::InformationTypeName(index))
                    .await?,
                id: read_b_varchar(reader, &mut budget, Field::InformationTypeId(index)).await?,
            });
        }

        let rank = match version {
            DataClassificationVersion::Version1 => None,
            DataClassificationVersion::Version2 => {
                Some(read_rank(reader, &mut budget, Field::ResultSetRank).await?)
            }
        };
        let property_bytes = match version {
            DataClassificationVersion::Version1 => 4,
            DataClassificationVersion::Version2 => 8,
        };

        let column_count = read_u16(reader, &mut budget, Field::ColumnCount).await?;
        budget.validate_collection(column_count, 2, Field::ColumnCount)?;
        let mut columns = Vec::with_capacity(usize::from(column_count));
        for column in 0..column_count {
            let count_field = Field::PropertyCount { column };
            let property_count = read_u16(reader, &mut budget, count_field).await?;
            budget.validate_collection(property_count, property_bytes, count_field)?;

            let mut properties = Vec::with_capacity(usize::from(property_count));
            for property in 0..property_count {
                let label_field = Field::LabelIndex { column, property };
                let label_index = read_u16(reader, &mut budget, label_field).await?;
                let information_type_field = Field::InformationTypeIndex { column, property };
                let information_type_index =
                    read_u16(reader, &mut budget, information_type_field).await?;
                let property_rank = match version {
                    DataClassificationVersion::Version1 => None,
                    DataClassificationVersion::Version2 => Some(
                        read_rank(
                            reader,
                            &mut budget,
                            Field::PropertyRank { column, property },
                        )
                        .await?,
                    ),
                };

                properties.push(SensitivityProperty {
                    sensitivity_label: label_reference(
                        label_index,
                        sensitivity_labels.len(),
                        label_field,
                    )?,
                    information_type: information_type_reference(
                        information_type_index,
                        information_types.len(),
                        information_type_field,
                    )?,
                    rank: property_rank,
                });
            }
            columns.push(ColumnSensitivity { properties });
        }

        Ok(DataClassificationMetadata {
            version,
            sensitivity_labels,
            information_types,
            rank,
            columns,
        })
    }
}

#[async_trait]
impl<T> TokenParser<T> for DataClassificationTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    /// The payload carries no version, so the layout comes from the version
    /// negotiated in FEATUREEXTACK. Without one the server sent a token it was
    /// never asked for, and the payload cannot be skipped safely.
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        let version = reader
            .negotiated_data_classification_version()
            .ok_or_else(|| {
                classification_error("token received but the feature was not negotiated")
            })?;
        self.parse_with_version(reader, version)
            .await
            .map(Tokens::DataClassification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::parsers::common::test_utils::MockReader;

    #[derive(Clone, Copy)]
    struct WireProperty {
        label: u16,
        information_type: u16,
        rank: i32,
    }

    fn append_b_varchar(bytes: &mut Vec<u8>, value: &str) {
        let units: Vec<_> = value.encode_utf16().collect();
        bytes.push(u8::try_from(units.len()).unwrap());
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }

    fn payload(
        version: DataClassificationVersion,
        labels: &[(&str, &str)],
        information_types: &[(&str, &str)],
        rank: i32,
        columns: &[Vec<WireProperty>],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u16::try_from(labels.len()).unwrap().to_le_bytes());
        for (name, id) in labels {
            append_b_varchar(&mut bytes, name);
            append_b_varchar(&mut bytes, id);
        }
        bytes.extend_from_slice(
            &u16::try_from(information_types.len())
                .unwrap()
                .to_le_bytes(),
        );
        for (name, id) in information_types {
            append_b_varchar(&mut bytes, name);
            append_b_varchar(&mut bytes, id);
        }
        if version == DataClassificationVersion::Version2 {
            bytes.extend_from_slice(&rank.to_le_bytes());
        }
        bytes.extend_from_slice(&u16::try_from(columns.len()).unwrap().to_le_bytes());
        for properties in columns {
            bytes.extend_from_slice(&u16::try_from(properties.len()).unwrap().to_le_bytes());
            for property in properties {
                bytes.extend_from_slice(&property.label.to_le_bytes());
                bytes.extend_from_slice(&property.information_type.to_le_bytes());
                if version == DataClassificationVersion::Version2 {
                    bytes.extend_from_slice(&property.rank.to_le_bytes());
                }
            }
        }
        bytes
    }

    async fn parse(
        bytes: Vec<u8>,
        version: DataClassificationVersion,
    ) -> TdsResult<DataClassificationMetadata> {
        DataClassificationTokenParser
            .parse_with_version(&mut MockReader::new(bytes), version)
            .await
    }

    #[tokio::test]
    async fn parses_minimum_v1_token() {
        let metadata = parse(
            payload(DataClassificationVersion::Version1, &[], &[], -1, &[]),
            DataClassificationVersion::Version1,
        )
        .await
        .unwrap();

        assert_eq!(metadata.version, DataClassificationVersion::Version1);
        assert!(metadata.sensitivity_labels.is_empty());
        assert!(metadata.information_types.is_empty());
        assert_eq!(metadata.rank, None);
        assert!(metadata.columns.is_empty());
    }

    #[tokio::test]
    async fn parses_minimum_v2_token() {
        let metadata = parse(
            payload(DataClassificationVersion::Version2, &[], &[], -1, &[]),
            DataClassificationVersion::Version2,
        )
        .await
        .unwrap();

        assert_eq!(metadata.version, DataClassificationVersion::Version2);
        assert_eq!(metadata.rank, Some(SensitivityRank::NotDefined));
        assert!(metadata.columns.is_empty());
    }

    #[tokio::test]
    async fn parses_dictionaries_columns_and_multiple_properties() {
        let columns = vec![
            vec![
                WireProperty {
                    label: 0,
                    information_type: 1,
                    rank: 30,
                },
                WireProperty {
                    label: 1,
                    information_type: 0,
                    rank: 20,
                },
            ],
            vec![],
            vec![WireProperty {
                label: NULL_DICTIONARY_INDEX,
                information_type: NULL_DICTIONARY_INDEX,
                rank: -1,
            }],
        ];
        let metadata = parse(
            payload(
                DataClassificationVersion::Version2,
                &[("Confidential", "L1"), ("Restricted", "L2")],
                &[("Financial", "I1"), ("Contact", "I2")],
                40,
                &columns,
            ),
            DataClassificationVersion::Version2,
        )
        .await
        .unwrap();

        assert_eq!(metadata.sensitivity_labels.len(), 2);
        assert_eq!(metadata.information_types.len(), 2);
        assert_eq!(metadata.columns.len(), 3);
        assert_eq!(metadata.columns[0].properties.len(), 2);
        assert!(!metadata.columns[1].is_classified());
        assert_eq!(
            metadata.columns[2].properties[0].sensitivity_label,
            SensitivityLabelReference::Unspecified
        );
        assert_eq!(
            metadata.columns[2].properties[0].information_type,
            InformationTypeReference::Unspecified
        );
    }

    #[tokio::test]
    async fn v1_does_not_consume_rank_fields() {
        let columns = vec![vec![WireProperty {
            label: 0,
            information_type: 0,
            rank: 40,
        }]];
        let metadata = parse(
            payload(
                DataClassificationVersion::Version1,
                &[("Label", "L")],
                &[("Type", "I")],
                40,
                &columns,
            ),
            DataClassificationVersion::Version1,
        )
        .await
        .unwrap();

        assert_eq!(metadata.rank, None);
        assert_eq!(metadata.columns[0].properties[0].rank, None);
    }

    #[tokio::test]
    async fn rejects_invalid_label_index() {
        let columns = vec![vec![WireProperty {
            label: 1,
            information_type: NULL_DICTIONARY_INDEX,
            rank: -1,
        }]];
        let error = parse(
            payload(
                DataClassificationVersion::Version1,
                &[("Label", "L")],
                &[],
                -1,
                &columns,
            ),
            DataClassificationVersion::Version1,
        )
        .await
        .unwrap_err();

        assert_single_classification_error(
            &error,
            "column 0 property 0 sensitivity-label index 1 is outside dictionary length 1",
        );
    }

    #[tokio::test]
    async fn rejects_invalid_information_type_index() {
        let columns = vec![vec![WireProperty {
            label: NULL_DICTIONARY_INDEX,
            information_type: 0,
            rank: -1,
        }]];
        let error = parse(
            payload(DataClassificationVersion::Version1, &[], &[], -1, &columns),
            DataClassificationVersion::Version1,
        )
        .await
        .unwrap_err();

        assert_single_classification_error(
            &error,
            "column 0 property 0 information-type index 0 is outside dictionary length 0",
        );
    }

    #[tokio::test]
    async fn rejects_invalid_utf16_in_label_name() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u16.to_le_bytes()); // label count
        bytes.push(1); // name: one UTF-16 code unit
        bytes.extend_from_slice(&0xD800u16.to_le_bytes()); // lone high surrogate
        bytes.push(0); // empty id
        bytes.extend_from_slice(&0u16.to_le_bytes()); // information-type count
        bytes.extend_from_slice(&0u16.to_le_bytes()); // column count

        let error = parse(bytes, DataClassificationVersion::Version1)
            .await
            .unwrap_err();

        assert_single_classification_error(&error, "invalid UTF-16 in sensitivity label 0 name");
    }

    #[tokio::test]
    async fn decodes_every_rank_from_signed_little_endian_wire_bytes() {
        let cases = [
            ([0xFF, 0xFF, 0xFF, 0xFF], SensitivityRank::NotDefined),
            ([0x00, 0x00, 0x00, 0x00], SensitivityRank::None),
            ([0x0A, 0x00, 0x00, 0x00], SensitivityRank::Low),
            ([0x14, 0x00, 0x00, 0x00], SensitivityRank::Medium),
            ([0x1E, 0x00, 0x00, 0x00], SensitivityRank::High),
            ([0x28, 0x00, 0x00, 0x00], SensitivityRank::Critical),
        ];
        for (rank_bytes, expected) in cases {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&0u16.to_le_bytes()); // label count
            bytes.extend_from_slice(&0u16.to_le_bytes()); // information-type count
            bytes.extend_from_slice(&rank_bytes); // result-set rank
            bytes.extend_from_slice(&1u16.to_le_bytes()); // column count
            bytes.extend_from_slice(&1u16.to_le_bytes()); // property count
            bytes.extend_from_slice(&NULL_DICTIONARY_INDEX.to_le_bytes());
            bytes.extend_from_slice(&NULL_DICTIONARY_INDEX.to_le_bytes());
            bytes.extend_from_slice(&rank_bytes); // property rank

            let metadata = parse(bytes, DataClassificationVersion::Version2)
                .await
                .unwrap();

            assert_eq!(metadata.rank, Some(expected));
            assert_eq!(metadata.columns[0].properties[0].rank, Some(expected));
        }
    }

    fn assert_single_classification_error(error: &Error, expected: &str) {
        let message = error.to_string();
        assert!(matches!(error, Error::ProtocolError(_)), "{message}");
        assert!(message.contains(expected), "{message}");
        assert_eq!(
            message.matches("DATACLASSIFICATION").count(),
            1,
            "{message}"
        );
    }

    fn assert_unexpected_eof(error: &Error) {
        assert!(
            matches!(error, Error::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "reader error was not passed through unchanged: {error}"
        );
    }

    #[tokio::test]
    async fn rejects_unknown_result_set_rank() {
        let error = parse(
            payload(DataClassificationVersion::Version2, &[], &[], 11, &[]),
            DataClassificationVersion::Version2,
        )
        .await
        .unwrap_err();

        assert_single_classification_error(&error, "result-set sensitivity rank: unknown value 11");
    }

    #[tokio::test]
    async fn rejects_unknown_property_rank() {
        let columns = vec![vec![WireProperty {
            label: NULL_DICTIONARY_INDEX,
            information_type: NULL_DICTIONARY_INDEX,
            rank: 11,
        }]];
        let error = parse(
            payload(DataClassificationVersion::Version2, &[], &[], -1, &columns),
            DataClassificationVersion::Version2,
        )
        .await
        .unwrap_err();

        assert_single_classification_error(
            &error,
            "column 0 property 0 sensitivity rank: unknown value 11",
        );
    }

    #[tokio::test]
    async fn rejects_v2_payload_missing_result_set_rank() {
        let bytes = payload(DataClassificationVersion::Version1, &[], &[], -1, &[]);
        let error = parse(bytes, DataClassificationVersion::Version2)
            .await
            .unwrap_err();

        assert_unexpected_eof(&error);
    }

    #[tokio::test]
    async fn token_parser_rejects_token_when_feature_not_negotiated() {
        let mut reader = MockReader::new(payload(
            DataClassificationVersion::Version1,
            &[],
            &[],
            -1,
            &[],
        ));
        let error = DataClassificationTokenParser
            .parse(&mut reader, &ParserContext::default())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not negotiated"));
    }

    #[tokio::test]
    async fn token_parser_uses_the_negotiated_version() {
        let columns = vec![vec![WireProperty {
            label: 0,
            information_type: NULL_DICTIONARY_INDEX,
            rank: 30,
        }]];
        for version in [
            DataClassificationVersion::Version1,
            DataClassificationVersion::Version2,
        ] {
            let bytes = payload(version, &[("Label", "L")], &[], 40, &columns);
            let mut reader = MockReader::new(bytes).with_data_classification_version(version);

            let token = DataClassificationTokenParser
                .parse(&mut reader, &ParserContext::default())
                .await
                .unwrap();

            let Tokens::DataClassification(metadata) = token else {
                panic!("expected a DataClassification token");
            };
            assert_eq!(metadata.version, version);
            let expected_rank = match version {
                DataClassificationVersion::Version1 => None,
                DataClassificationVersion::Version2 => Some(SensitivityRank::High),
            };
            assert_eq!(metadata.columns[0].properties[0].rank, expected_rank);
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[tokio::test]
    async fn rejects_counts_that_exceed_parser_budget_before_allocating() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());

        let error = parse(bytes, DataClassificationVersion::Version1)
            .await
            .unwrap_err();

        assert_single_classification_error(
            &error,
            "column 0 sensitivity-property count 65535 requires at least 262140 bytes, \
             exceeding the remaining parser limit",
        );
    }

    #[tokio::test]
    async fn string_content_counts_toward_the_parser_budget() {
        // 256 full labels (255-unit name and id) plus a 252-unit trailing name
        // fill the budget exactly: 6 + 256 * 1022 + 2 + 504 = 262144.
        fn labels(last_name_units: usize) -> Vec<(String, String)> {
            let full = "a".repeat(255);
            let mut labels = vec![(full.clone(), full); 256];
            labels.push(("b".repeat(last_name_units), String::new()));
            labels
        }
        async fn parse_labels(
            labels: &[(String, String)],
        ) -> TdsResult<DataClassificationMetadata> {
            let labels: Vec<_> = labels
                .iter()
                .map(|(name, id)| (name.as_str(), id.as_str()))
                .collect();
            let bytes = payload(DataClassificationVersion::Version1, &labels, &[], -1, &[]);
            parse(bytes, DataClassificationVersion::Version1).await
        }

        let at_limit = labels(252);
        let metadata = parse_labels(&at_limit).await.unwrap();
        assert_eq!(metadata.sensitivity_labels.len(), 257);

        let error = parse_labels(&labels(255)).await.unwrap_err();
        assert_single_classification_error(
            &error,
            "sensitivity label 256 name exceeds the maximum supported token size of 262144 bytes",
        );
    }

    #[tokio::test]
    async fn every_truncated_prefix_passes_the_reader_error_through() {
        let columns = vec![vec![WireProperty {
            label: 0,
            information_type: 0,
            rank: 30,
        }]];
        let complete = payload(
            DataClassificationVersion::Version2,
            &[("Confidential", "label-id")],
            &[("Financial", "type-id")],
            40,
            &columns,
        );

        for prefix_len in 0..complete.len() {
            let error = parse(
                complete[..prefix_len].to_vec(),
                DataClassificationVersion::Version2,
            )
            .await
            .expect_err("truncated payload unexpectedly parsed");
            assert_unexpected_eof(&error);
        }
    }

    #[tokio::test]
    async fn parser_leaves_bytes_after_the_self_describing_token() {
        let mut bytes = payload(DataClassificationVersion::Version1, &[], &[], -1, &[]);
        bytes.extend_from_slice(&[0xFD, 0xAA]);
        let mut reader = MockReader::new(bytes);

        DataClassificationTokenParser
            .parse_with_version(&mut reader, DataClassificationVersion::Version1)
            .await
            .unwrap();

        assert_eq!(reader.remaining(), 2);
    }
}
