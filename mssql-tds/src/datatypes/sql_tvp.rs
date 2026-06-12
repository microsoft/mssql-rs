// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Table-Valued Parameter (TVP) types for RPC calls.
//!
//! A TVP (TDS type `0xF3`) sends tabular data as a single, input-only RPC
//! parameter. This module defines the Rust-side representation of a TVP:
//! its three-part type name, per-column metadata, optional sort/unique
//! hints, and the row data. Serialization into TDS wire format is added in
//! a later phase.
//!
//! The column type template reuses [`SqlType`] with `None` values (e.g.
//! `SqlType::Int(None)` describes an `int` column), avoiding a parallel type
//! hierarchy. For types whose metadata lives inside the value `Option`
//! (`Decimal`, `Numeric`, `Time`, `DateTime2`, `DateTimeOffset`),
//! [`TvpColumnDef`] carries explicit `precision`/`scale` overrides.

use bitflags::bitflags;

use crate::core::TdsResult;
use crate::datatypes::sqltypes::SqlType;
use crate::datatypes::tds_value_serializer::TdsValueSerializer;
use crate::error::Error;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::token::tokens::SqlCollation;

/// Token written before each TVP row (`TVP_ROW`).
pub(crate) const TVP_ROW_TOKEN: u8 = 0x01;

/// Token terminating optional metadata and the row set (`TVP_END`).
pub(crate) const TVP_END_TOKEN: u8 = 0x00;

/// Token introducing the order/unique optional metadata (`TVP_ORDER_UNIQUE`).
pub(crate) const TVP_ORDER_UNIQUE_TOKEN: u8 = 0x10;

/// Column-count sentinel indicating a TVP with no column metadata
/// (`TVP_NOMETADATA`), used for NULL TVPs.
pub(crate) const TVP_NOMETADATA_TOKEN: u16 = 0xFFFF;

/// Maximum number of columns SQL Server allows in a table type.
const MAX_TVP_COLUMNS: usize = 1024;

bitflags! {
    /// Per-column flags in TVP column metadata.
    ///
    /// [`NULLABLE`](Self::NULLABLE) is always set on the wire; per-cell
    /// null-ness is governed by the row data, not this flag.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TvpColumnFlags: u16 {
        /// Column accepts `NULL` values.
        const NULLABLE = 0x0001;
        /// Column has a default value (server supplies it when omitted).
        const DEFAULT = 0x0200;
    }
}

bitflags! {
    /// Sort order and uniqueness flags for a TVP order/unique hint.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TvpOrderFlags: u8 {
        /// Column is sorted ascending.
        const ASC = 0x01;
        /// Column is sorted descending.
        const DESC = 0x02;
        /// Column participates in a unique key.
        const UNIQUE = 0x04;
    }
}

/// The three-part name of a table type (`db.schema.type`).
///
/// `db_name` (catalog) defaults to `None`: SQL Server forbids cross-database
/// TVP types, but the field is still emitted on the wire (as `0x00` when
/// absent) for spec compliance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvpTypeName {
    /// Catalog/database name. Always `None` in practice.
    pub db_name: Option<String>,
    /// Schema name (e.g. `dbo`).
    pub schema_name: Option<String>,
    /// Table type name.
    pub type_name: String,
}

impl TvpTypeName {
    /// Creates a schema-qualified type name (catalog left empty).
    pub fn new(schema_name: Option<String>, type_name: String) -> Self {
        Self {
            db_name: None,
            schema_name,
            type_name,
        }
    }

    /// Validates that the type name is usable on the wire.
    ///
    /// The type name part must be non-empty; SQL Server cannot resolve a TVP
    /// without it.
    pub(crate) fn validate(&self) -> TdsResult<()> {
        if self.type_name.is_empty() {
            return Err(Error::UsageError(
                "TVP type name must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

/// Metadata for a single TVP column.
///
/// `column_type` is a [`SqlType`] with a `None` value used purely as a type
/// template. For `Decimal`, `Numeric`, `Time`, `DateTime2`, and
/// `DateTimeOffset`, whose precision/scale live inside the value `Option`,
/// the `precision`/`scale` overrides supply that metadata; they are ignored
/// for other types.
#[derive(Debug, Clone, PartialEq)]
pub struct TvpColumnDef {
    /// Column type template (a [`SqlType`] with a `None` value).
    pub column_type: SqlType,
    /// Per-column flags. `NULLABLE` is always set during serialization.
    pub flags: TvpColumnFlags,
    /// Precision override for `Decimal`/`Numeric` columns.
    pub precision: Option<u8>,
    /// Scale override for `Decimal`/`Numeric`/`Time`/`DateTime2`/
    /// `DateTimeOffset` columns.
    pub scale: Option<u8>,
}

impl TvpColumnDef {
    /// Creates a column definition from a type template, with no
    /// precision/scale overrides and only the `NULLABLE` flag set.
    pub fn new(column_type: SqlType) -> Self {
        Self {
            column_type,
            flags: TvpColumnFlags::NULLABLE,
            precision: None,
            scale: None,
        }
    }
}

/// An order/unique hint for a single TVP column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvpOrderHint {
    /// 1-based ordinal of the column this hint applies to.
    pub column_ordinal: u16,
    /// Sort order and uniqueness flags.
    pub flags: TvpOrderFlags,
}

/// The column metadata, row data, and optional hints of a non-NULL TVP.
///
/// A NULL TVP is represented by `SqlType::Table(name, None)`; this struct is
/// the payload for the `Some` case. An empty `rows` vector yields a valid
/// empty TVP.
#[derive(Debug, Clone, PartialEq)]
pub struct TvpTableData {
    /// Column definitions (the table type's schema).
    pub columns: Vec<TvpColumnDef>,
    /// Row data; each row holds one [`SqlType`] value per column.
    pub rows: Vec<Vec<SqlType>>,
    /// Optional sort/unique hints.
    pub order_hints: Vec<TvpOrderHint>,
}

impl TvpTableData {
    /// Creates table data from column definitions and rows, with no hints.
    pub fn new(columns: Vec<TvpColumnDef>, rows: Vec<Vec<SqlType>>) -> Self {
        Self {
            columns,
            rows,
            order_hints: Vec::new(),
        }
    }

    /// Validates the table data against TVP wire-format constraints.
    ///
    /// Checks that there is at least one column and no more than
    /// [`MAX_TVP_COLUMNS`], that every row has exactly one value per column,
    /// and that each cell's [`SqlType`] variant matches its column's type
    /// template. Variant matching uses [`std::mem::discriminant`], so the
    /// inner value (including `None` vs `Some`) is not compared.
    pub(crate) fn validate(&self) -> TdsResult<()> {
        if self.columns.is_empty() {
            return Err(Error::UsageError(
                "TVP must have at least one column".to_string(),
            ));
        }
        if self.columns.len() > MAX_TVP_COLUMNS {
            return Err(Error::UsageError(format!(
                "TVP has {} columns but SQL Server allows at most {MAX_TVP_COLUMNS}",
                self.columns.len()
            )));
        }

        for (row_idx, row) in self.rows.iter().enumerate() {
            if row.len() != self.columns.len() {
                return Err(Error::UsageError(format!(
                    "TVP row {row_idx} has {} values but the table type has {} columns",
                    row.len(),
                    self.columns.len()
                )));
            }

            for (col_idx, (column, cell)) in self.columns.iter().zip(row.iter()).enumerate() {
                if std::mem::discriminant(cell) != std::mem::discriminant(&column.column_type) {
                    return Err(Error::UsageError(format!(
                        "TVP row {row_idx} column {col_idx} type mismatch: cell is {cell:?} \
                         but the column is declared as {:?}",
                        column.column_type
                    )));
                }
            }
        }

        Ok(())
    }
}

/// Writes the three-part TVP type name (`db.schema.type`) as B_VARCHARs.
///
/// Each part is a `u8` UTF-16 character count followed by the UTF-16LE bytes;
/// an absent or empty part is a single `0x00` byte.
pub(crate) async fn write_tvp_type_name(
    packet_writer: &mut PacketWriter<'_>,
    type_name: &TvpTypeName,
) -> TdsResult<()> {
    write_b_varchar(packet_writer, type_name.db_name.as_deref()).await?;
    write_b_varchar(packet_writer, type_name.schema_name.as_deref()).await?;
    write_b_varchar(packet_writer, Some(type_name.type_name.as_str())).await?;
    Ok(())
}

/// Writes a TDS B_VARCHAR: a `u8` UTF-16 character count followed by the
/// UTF-16LE-encoded characters. `None` or an empty string writes a single
/// `0x00` length byte.
async fn write_b_varchar(
    packet_writer: &mut PacketWriter<'_>,
    value: Option<&str>,
) -> TdsResult<()> {
    match value {
        Some(s) if !s.is_empty() => {
            let char_count = s.encode_utf16().count();
            if char_count > u8::MAX as usize {
                return Err(Error::UsageError(format!(
                    "TVP name part is too long: {char_count} UTF-16 code units (max 255)"
                )));
            }
            packet_writer.write_byte_async(char_count as u8).await?;
            packet_writer.write_string_unicode_async(s).await?;
        }
        _ => {
            packet_writer.write_byte_async(0).await?;
        }
    }
    Ok(())
}

/// Writes the TVP column metadata block: the column count (USHORT) followed by
/// per-column `UserType` (DWORD), `Flags` (USHORT), `TYPE_INFO`, and a
/// zero-length column name.
///
/// The end-of-metadata token is written separately by [`write_tvp_order_unique`].
pub(crate) async fn write_tvp_column_metadata(
    packet_writer: &mut PacketWriter<'_>,
    columns: &[TvpColumnDef],
    db_collation: &SqlCollation,
) -> TdsResult<()> {
    if columns.len() > u16::MAX as usize {
        return Err(Error::UsageError(format!(
            "TVP has too many columns: {} (max {})",
            columns.len(),
            u16::MAX
        )));
    }

    packet_writer.write_u16_async(columns.len() as u16).await?;

    for column in columns {
        // Legacy LOB types are not permitted in table types.
        if matches!(column.column_type, SqlType::Text(_) | SqlType::NText(_)) {
            return Err(Error::UsageError(
                "Legacy LOB types (text/ntext) are not allowed as TVP columns; \
                 use varchar(max)/nvarchar(max) instead"
                    .to_string(),
            ));
        }

        // UserType: 4-byte DWORD, always 0 for TVP columns.
        packet_writer.write_u32_async(0).await?;

        // Flags: 2-byte USHORT. NULLABLE is always set; per-cell null-ness is
        // governed by the row data, not this flag.
        let flags = column.flags | TvpColumnFlags::NULLABLE;
        packet_writer.write_u16_async(flags.bits()).await?;

        // TYPE_INFO (type byte + length/precision/scale/collation).
        column
            .column_type
            .write_type_info(packet_writer, db_collation, column.precision, column.scale)
            .await?;

        // Column name: zero-length (TVP columns are positional).
        packet_writer.write_byte_async(0).await?;
    }

    Ok(())
}

/// Writes the optional order/unique metadata, then the token that terminates
/// the column-metadata section.
///
/// When `order_hints` is non-empty, emits the [`TVP_ORDER_UNIQUE_TOKEN`]
/// (`0x10`), a USHORT count, and a `(ordinal: USHORT, flags: u8)` pair per
/// hint. The trailing [`TVP_END_TOKEN`] (`0x00`) that ends the metadata is
/// always written, even when there are no hints.
pub(crate) async fn write_tvp_order_unique(
    packet_writer: &mut PacketWriter<'_>,
    order_hints: &[TvpOrderHint],
) -> TdsResult<()> {
    if !order_hints.is_empty() {
        packet_writer
            .write_byte_async(TVP_ORDER_UNIQUE_TOKEN)
            .await?;
        packet_writer
            .write_u16_async(order_hints.len() as u16)
            .await?;
        for hint in order_hints {
            packet_writer.write_u16_async(hint.column_ordinal).await?;
            packet_writer.write_byte_async(hint.flags.bits()).await?;
        }
    }

    // End of the optional metadata section.
    packet_writer.write_byte_async(TVP_END_TOKEN).await?;
    Ok(())
}

/// Writes the TVP row data: a [`TVP_ROW_TOKEN`] (`0x01`) before each row,
/// followed by one value per column, then the trailing [`TVP_END_TOKEN`]
/// (`0x00`) that ends the row set.
///
/// Each value's [`TdsTypeContext`](crate::datatypes::tds_value_serializer::TdsTypeContext)
/// is derived from the **column definition** (not the cell value) so that the
/// wire encoding stays consistent with the column metadata across every row;
/// the encoded value bytes come from the cell.
pub(crate) async fn write_tvp_rows(
    packet_writer: &mut PacketWriter<'_>,
    columns: &[TvpColumnDef],
    rows: &[Vec<SqlType>],
    db_collation: &SqlCollation,
) -> TdsResult<()> {
    for row in rows {
        if row.len() != columns.len() {
            return Err(Error::UsageError(format!(
                "TVP row has {} values but the table type has {} columns",
                row.len(),
                columns.len()
            )));
        }

        packet_writer.write_byte_async(TVP_ROW_TOKEN).await?;

        for (column, cell) in columns.iter().zip(row.iter()) {
            // Context from the column definition keeps the encoding aligned with
            // the declared metadata; the value bytes come from the cell.
            let (_, ctx) = column.column_type.to_column_value_and_context(db_collation);
            let (value, _) = cell.to_column_value_and_context(db_collation);
            TdsValueSerializer::serialize_value(packet_writer, &value, &ctx).await?;
        }
    }

    // End of the row set.
    packet_writer.write_byte_async(TVP_END_TOKEN).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::packet_reader::tests::MockNetworkReaderWriter;
    use crate::io::packet_writer::PacketWriter;
    use crate::message::messages::PacketType;

    fn default_collation() -> SqlCollation {
        SqlCollation {
            info: 0,
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        }
    }

    /// Serializes a value through `SqlType::serialize` and returns the payload
    /// bytes that follow the TDS packet header.
    async fn serialize_payload(value: &SqlType) -> Vec<u8> {
        let collation = default_collation();
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        value.serialize(&mut writer, &collation).await.unwrap();
        writer.finalize().await.unwrap();
        let payload = mock.get_written_data();
        payload[PacketWriter::PACKET_HEADER_SIZE..].to_vec()
    }

    #[tokio::test]
    async fn test_write_tvp_type_name_bytes() {
        let name = TvpTypeName::new(Some("dbo".to_string()), "MyType".to_string());
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        write_tvp_type_name(&mut writer, &name).await.unwrap();
        writer.finalize().await.unwrap();
        let payload = mock.get_written_data();
        let bytes = &payload[PacketWriter::PACKET_HEADER_SIZE..];

        let expected: Vec<u8> = vec![
            0x00, // db_name: empty
            0x03, b'd', 0x00, b'b', 0x00, b'o', 0x00, // schema "dbo"
            0x06, b'M', 0x00, b'y', 0x00, b'T', 0x00, b'y', 0x00, b'p', 0x00, b'e',
            0x00, // type "MyType"
        ];
        assert_eq!(bytes, expected.as_slice());
    }

    #[tokio::test]
    async fn test_serialize_tvp_single_int_row() {
        let table = TvpTableData::new(
            vec![TvpColumnDef::new(SqlType::Int(None))],
            vec![vec![SqlType::Int(Some(0x0102_0304))]],
        );
        let value = SqlType::Table(
            TvpTypeName::new(Some("dbo".to_string()), "MyType".to_string()),
            Some(table),
        );

        let bytes = serialize_payload(&value).await;

        let expected: Vec<u8> = vec![
            0xF3, // TVP type byte
            // 3-part name: "", "dbo", "MyType"
            0x00, 0x03, b'd', 0x00, b'b', 0x00, b'o', 0x00, 0x06, b'M', 0x00, b'y', 0x00, b'T',
            0x00, b'y', 0x00, b'p', 0x00, b'e', 0x00, // column count = 1
            0x01, 0x00, // UserType DWORD = 0
            0x00, 0x00, 0x00, 0x00, // Flags = NULLABLE (0x0001)
            0x01, 0x00, // TYPE_INFO: IntN (0x26) + size 4
            0x26, 0x04, // column name length = 0
            0x00, // end of metadata (no order/unique)
            0x00, // row token
            0x01, // int value: length 4 + 0x01020304 little-endian
            0x04, 0x04, 0x03, 0x02, 0x01, // end of rows
            0x00,
        ];
        assert_eq!(bytes, expected);
    }

    #[tokio::test]
    async fn test_serialize_null_tvp() {
        let value = SqlType::Table(
            TvpTypeName::new(Some("dbo".to_string()), "MyType".to_string()),
            None,
        );

        let bytes = serialize_payload(&value).await;

        let expected: Vec<u8> = vec![
            0xF3, // TVP type byte
            // 3-part name: "", "dbo", "MyType"
            0x00, 0x03, b'd', 0x00, b'b', 0x00, b'o', 0x00, 0x06, b'M', 0x00, b'y', 0x00, b'T',
            0x00, b'y', 0x00, b'p', 0x00, b'e', 0x00, // TVP_NOMETADATA column count
            0xFF, 0xFF, // end of metadata, end of rows
            0x00, 0x00,
        ];
        assert_eq!(bytes, expected);
    }

    /// Captures the bytes written by [`write_tvp_order_unique`] for the given
    /// hints (no surrounding TVP framing).
    async fn order_unique_bytes(order_hints: &[TvpOrderHint]) -> Vec<u8> {
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        write_tvp_order_unique(&mut writer, order_hints)
            .await
            .unwrap();
        writer.finalize().await.unwrap();
        let payload = mock.get_written_data();
        payload[PacketWriter::PACKET_HEADER_SIZE..].to_vec()
    }

    #[tokio::test]
    async fn test_write_tvp_order_unique_no_hints() {
        // With no hints, only the end-of-metadata token is written.
        let bytes = order_unique_bytes(&[]).await;
        assert_eq!(bytes, vec![TVP_END_TOKEN]);
    }

    #[tokio::test]
    async fn test_write_tvp_order_unique_bytes() {
        // ASC | UNIQUE on 1-based column ordinal 1. The exact byte layout must
        // match the reference implementation (.NET SqlClient WriteTvpOrderUnique):
        // token, USHORT count, then per hint USHORT ordinal + BYTE flags,
        // followed by the end-of-metadata token.
        let hints = vec![TvpOrderHint {
            column_ordinal: 1,
            flags: TvpOrderFlags::ASC | TvpOrderFlags::UNIQUE,
        }];
        let bytes = order_unique_bytes(&hints).await;

        let expected: Vec<u8> = vec![
            TVP_ORDER_UNIQUE_TOKEN, // 0x10
            0x01,
            0x00, // count = 1 (USHORT, little-endian)
            0x01,
            0x00,          // column ordinal = 1 (USHORT, little-endian)
            0x05,          // flags = ASC (0x01) | UNIQUE (0x04)
            TVP_END_TOKEN, // 0x00 end-of-metadata
        ];
        assert_eq!(bytes, expected);
    }

    #[tokio::test]
    async fn test_write_tvp_order_unique_multiple_hints() {
        // Two hints exercise the count and per-hint repetition: column 1 DESC,
        // column 2 ASC|UNIQUE.
        let hints = vec![
            TvpOrderHint {
                column_ordinal: 1,
                flags: TvpOrderFlags::DESC,
            },
            TvpOrderHint {
                column_ordinal: 2,
                flags: TvpOrderFlags::ASC | TvpOrderFlags::UNIQUE,
            },
        ];
        let bytes = order_unique_bytes(&hints).await;

        let expected: Vec<u8> = vec![
            TVP_ORDER_UNIQUE_TOKEN, // 0x10
            0x02,
            0x00, // count = 2
            0x01,
            0x00,
            0x02, // ordinal 1, flags DESC (0x02)
            0x02,
            0x00,
            0x05,          // ordinal 2, flags ASC|UNIQUE (0x05)
            TVP_END_TOKEN, // 0x00 end-of-metadata
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn test_validate_empty_type_name_rejected() {
        let name = TvpTypeName::new(Some("dbo".to_string()), String::new());
        assert!(matches!(name.validate(), Err(Error::UsageError(_))));
    }

    #[test]
    fn test_validate_type_name_ok() {
        let name = TvpTypeName::new(None, "MyType".to_string());
        assert!(name.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_columns_rejected() {
        let data = TvpTableData::new(Vec::new(), Vec::new());
        assert!(matches!(data.validate(), Err(Error::UsageError(_))));
    }

    #[test]
    fn test_validate_too_many_columns_rejected() {
        let columns = (0..MAX_TVP_COLUMNS + 1)
            .map(|_| TvpColumnDef::new(SqlType::Int(None)))
            .collect();
        let data = TvpTableData::new(columns, Vec::new());
        assert!(matches!(data.validate(), Err(Error::UsageError(_))));
    }

    #[test]
    fn test_validate_row_length_mismatch_rejected() {
        let data = TvpTableData::new(
            vec![
                TvpColumnDef::new(SqlType::Int(None)),
                TvpColumnDef::new(SqlType::Int(None)),
            ],
            vec![vec![SqlType::Int(Some(1))]],
        );
        assert!(matches!(data.validate(), Err(Error::UsageError(_))));
    }

    #[test]
    fn test_validate_cell_type_mismatch_rejected() {
        let data = TvpTableData::new(
            vec![TvpColumnDef::new(SqlType::Int(None))],
            vec![vec![SqlType::BigInt(Some(1))]],
        );
        assert!(matches!(data.validate(), Err(Error::UsageError(_))));
    }

    #[test]
    fn test_validate_matching_data_ok() {
        let data = TvpTableData::new(
            vec![TvpColumnDef::new(SqlType::Int(None))],
            vec![vec![SqlType::Int(Some(1))], vec![SqlType::Int(None)]],
        );
        assert!(data.validate().is_ok());
    }

    /// A name part exceeding the B_VARCHAR `u8` character-count limit (255) is
    /// rejected at serialization time; `TvpTypeName::validate` does not bound
    /// length, so this guard in `write_b_varchar` is the only check.
    #[tokio::test]
    async fn test_write_tvp_type_name_part_too_long_rejected() {
        let name = TvpTypeName::new(Some("dbo".to_string()), "a".repeat(256));
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let result = write_tvp_type_name(&mut writer, &name).await;
        assert!(matches!(result, Err(Error::UsageError(_))));
    }

    /// `write_tvp_column_metadata` rejects column counts that overflow the
    /// USHORT column-count field.
    #[tokio::test]
    async fn test_write_tvp_column_metadata_too_many_columns_rejected() {
        let columns: Vec<TvpColumnDef> = (0..=u16::MAX as usize)
            .map(|_| TvpColumnDef::new(SqlType::Int(None)))
            .collect();
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let result = write_tvp_column_metadata(&mut writer, &columns, &default_collation()).await;
        assert!(matches!(result, Err(Error::UsageError(_))));
    }

    /// Legacy LOB types (`text`/`ntext`) are not valid TVP column types and are
    /// rejected while writing column metadata.
    #[tokio::test]
    async fn test_write_tvp_column_metadata_lob_column_rejected() {
        let columns = vec![TvpColumnDef::new(SqlType::Text(None))];
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let result = write_tvp_column_metadata(&mut writer, &columns, &default_collation()).await;
        assert!(matches!(result, Err(Error::UsageError(_))));
    }

    /// A row whose value count differs from the column count is rejected while
    /// writing the row set.
    #[tokio::test]
    async fn test_write_tvp_rows_length_mismatch_rejected() {
        let columns = vec![
            TvpColumnDef::new(SqlType::Int(None)),
            TvpColumnDef::new(SqlType::Int(None)),
        ];
        let rows = vec![vec![SqlType::Int(Some(1))]];
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let result = write_tvp_rows(&mut writer, &columns, &rows, &default_collation()).await;
        assert!(matches!(result, Err(Error::UsageError(_))));
    }

    /// A TVP is never a column type within another type, so `write_type_info`
    /// must reject it; serialization is dispatched via `serialize_table`.
    #[tokio::test]
    async fn test_write_type_info_on_table_rejected() {
        let value = SqlType::Table(
            TvpTypeName::new(Some("dbo".to_string()), "MyType".to_string()),
            None,
        );
        let mut mock = MockNetworkReaderWriter::default();
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let result = value
            .write_type_info(&mut writer, &default_collation(), None, None)
            .await;
        assert!(matches!(result, Err(Error::ImplementationError(_))));
    }

    /// A TVP has no scalar `ColumnValues` counterpart; the fallback arm yields a
    /// `Null` placeholder (real serialization goes through `serialize_table`).
    #[test]
    fn test_table_to_column_value_is_null() {
        use crate::datatypes::column_values::ColumnValues;

        let value = SqlType::Table(TvpTypeName::new(None, "MyType".to_string()), None);
        let (cv, _) = value.to_column_value_and_context(&default_collation());
        assert!(matches!(cv, ColumnValues::Null));
    }
}
