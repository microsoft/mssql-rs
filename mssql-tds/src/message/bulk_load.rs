// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bulk load message implementation for SQL Server bulk copy protocol.
//!
//! This module implements the TDS bulk load protocol for high-performance data insertion.
//! It follows the .NET SqlBulkCopy implementation pattern from TdsParser.WriteBulkCopyMetaData
//! and WriteBulkCopyValue methods.

use crate::connection::bulk_copy::{BulkCopyOptions, BulkLoadRow};
use crate::core::TdsResult;
use crate::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;
use crate::datatypes::column_values::ColumnValues;
use crate::datatypes::sqldatatypes::TdsDataType;
use crate::datatypes::tds_value_serializer::{TdsTypeContext, TdsValueSerializer};
use crate::error::Error;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter, TdsPacketWriterUnchecked};
use crate::token::tokens::SqlCollation;
use tracing::{debug, trace};

// TDS Token types
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ROW: u8 = 0xD1;
const TOKEN_NBCROW: u8 = 0xD2; // Null Bitmap Compressed Row
const TOKEN_DONE: u8 = 0xFD;

// NULL markers for different type classes
const FIXEDNULL: u8 = 0x00;
const VARNULL: u16 = 0xFFFF;
// PLP constants imported from tds_value_serializer

/// Streaming bulk load writer for transmitting bulk copy data row-by-row.
///
/// This writer enables streaming bulk copy without accumulating rows in memory.
/// It follows the .NET SqlBulkCopy streaming pattern where rows are written
/// directly to the TDS protocol stream as they are read from the source.
///
/// # Usage Flow
///
/// 1. Create writer with `new()`
/// 2. Call `begin()` to write COLMETADATA token
/// 3. Call `write_row_zerocopy()` for each row (streamed, not buffered)
/// 4. Call `end()` to write DONE token and finalize
pub struct StreamingBulkLoadWriter<'a> {
    /// Packet writer for TDS protocol
    packet_writer: &'a mut PacketWriter<'a>,

    /// Destination table name (for error messages)
    table_name: String,

    /// Column metadata
    column_metadata: Vec<BulkCopyColumnMetadata>,

    /// Bulk copy options
    options: BulkCopyOptions,

    /// Connection's default collation (used when column metadata doesn't specify collation)
    default_collation: SqlCollation,

    /// Whether metadata has been written
    metadata_written: bool,

    /// Number of rows written so far
    rows_written: u64,

    /// Pre-created type contexts for each column (initialized during begin())
    /// This avoids allocating contexts per column per row
    column_contexts: Vec<TdsTypeContext>,

    /// Column count from the first row (None until first row is written)
    /// This is used to validate that all subsequent rows have the same column count
    first_row_column_count: Option<usize>,
}

impl<'a> StreamingBulkLoadWriter<'a> {
    /// Create a new streaming bulk load writer.
    ///
    /// # Arguments
    ///
    /// * `packet_writer` - TDS packet writer
    /// * `table_name` - Destination table name                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               
    /// * `column_metadata` - Column metadata for the bulk load
    /// * `options` - Bulk copy options
    /// * `default_collation` - Connection's default collation (used when column metadata doesn't specify collation)
    pub fn new(
        packet_writer: &'a mut PacketWriter<'a>,
        table_name: String,
        column_metadata: Vec<BulkCopyColumnMetadata>,
        options: BulkCopyOptions,
        default_collation: SqlCollation,
    ) -> Self {
        Self {
            packet_writer,
            table_name,
            column_metadata,
            options,
            default_collation,
            metadata_written: false,
            rows_written: 0,
            column_contexts: Vec::new(),  // Will be populated in begin()
            first_row_column_count: None, // Will be set when first row is written
        }
    }

    /// Begin streaming - write COLMETADATA token.
    ///
    /// This must be called before any rows can be written.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Metadata has already been written
    /// - Network errors occur during transmission
    pub async fn begin(&mut self) -> TdsResult<()> {
        if self.metadata_written {
            return Err(Error::ProtocolError(
                "Metadata already written - cannot call begin() twice".to_string(),
            ));
        }

        // Pre-create type contexts for all columns (one-time allocation)
        // This avoids creating contexts per column per row
        self.column_contexts.clear();
        self.column_contexts.reserve(self.column_metadata.len());

        for col_meta in &self.column_metadata {
            // CRITICAL: For NVARCHAR/NCHAR types, max_size must be in CHARACTERS, not bytes!
            // SQL Server's metadata returns byte length (e.g., 8000 for NVARCHAR(4000)),
            // but TDS wire format uses character count for length prefixes.
            // For NVARCHAR: character_count = byte_length / 2
            // For VARCHAR: character_count = byte_length (same as bytes)
            let max_size = match col_meta.tds_type {
                0xE7 | 0xEF => {
                    // NVARCHAR(n) or NCHAR(n): Convert byte length to character count
                    // Each UTF-16 character is 2 bytes
                    // For PLP types (NVARCHAR(MAX)), use length as-is (0xFFFF sentinel)
                    if col_meta.length_type.is_plp() {
                        col_meta.length as usize
                    } else {
                        (col_meta.length / 2) as usize
                    }
                }
                _ => {
                    // All other types: Use length as-is
                    col_meta.length as usize
                }
            };

            let ctx = TdsTypeContext {
                tds_type: col_meta.tds_type,
                max_size,
                is_plp: col_meta.length_type.is_plp(),
                is_fixed_length: col_meta.length_type.is_fixed(),
                precision: if col_meta.precision > 0 {
                    Some(col_meta.precision)
                } else {
                    None
                },
                scale: if col_meta.scale > 0 {
                    Some(col_meta.scale)
                } else {
                    None
                },
                collation: col_meta.collation,
                is_nullable: col_meta.is_nullable,
            };
            self.column_contexts.push(ctx);
        }

        // Write COLMETADATA token and column descriptors
        // This is the same logic as BulkLoadMessage::write_metadata
        self.write_metadata_internal().await?;
        self.metadata_written = true;

        trace!(
            "StreamingBulkLoadWriter: Metadata written for {} columns",
            self.column_metadata.len()
        );

        Ok(())
    }

    /// Write a single column value directly (for zero-copy bulk load).
    ///
    /// This is used by the `BulkLoadRow` trait to write columns one at a time
    /// without allocating a Vec<ColumnValues>.
    ///
    /// # Arguments
    ///
    /// * `column_index` - The index of the column being written
    /// * `value` - Column value to write
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Column index is out of bounds
    /// - Network errors occur during transmission
    /// - Type conversion errors occur
    pub async fn write_column_value(
        &mut self,
        column_index: usize,
        value: &ColumnValues,
    ) -> TdsResult<()> {
        // Get the context for the specified column
        let ctx = self.column_contexts.get(column_index).ok_or_else(|| {
            Error::UsageError(format!(
                "Column index {} out of bounds, expected {} columns based on table metadata. All rows must have the same number of columns as the first row.",
                column_index,
                self.column_contexts.len()
            ))
        })?;

        TdsValueSerializer::serialize_value(self.packet_writer, value, ctx).await?;

        Ok(())
    }

    /// Get mutable access to the packet writer (for pre-serialized bytes).
    ///
    /// This allows external code to write pre-serialized TDS bytes directly
    /// to the packet without going through write_column_value.
    ///
    /// # Safety
    ///
    /// Caller must ensure the bytes written are valid TDS wire format for
    /// the expected column types, or SQL Server will reject the data.
    pub fn packet_writer(&mut self) -> &mut PacketWriter<'a> {
        self.packet_writer
    }

    /// Write an i32 value directly for a column, bypassing ColumnValues dispatch.
    pub async fn write_int32(&mut self, column_index: usize, value: i32) -> TdsResult<()> {
        let ctx = &self.column_contexts[column_index];
        TdsValueSerializer::serialize_int(self.packet_writer, value, ctx).await
    }

    /// Write an i64 value directly for a column, bypassing ColumnValues dispatch.
    pub async fn write_int64(&mut self, column_index: usize, value: i64) -> TdsResult<()> {
        let ctx = &self.column_contexts[column_index];
        TdsValueSerializer::serialize_bigint(self.packet_writer, value, ctx).await
    }

    /// Write an f64 value directly for a column, bypassing ColumnValues dispatch.
    pub async fn write_float64(&mut self, column_index: usize, value: f64) -> TdsResult<()> {
        let ctx = &self.column_contexts[column_index];
        TdsValueSerializer::serialize_float(self.packet_writer, value, ctx).await
    }

    /// Write a UTF-8 string as NVARCHAR, bypassing ColumnValues/SqlString.
    pub async fn write_nvarchar_str(&mut self, column_index: usize, value: &str) -> TdsResult<()> {
        let ctx = &self.column_contexts[column_index];
        TdsValueSerializer::serialize_string_utf16(self.packet_writer, value, ctx).await
    }

    /// Write a decimal value directly from parts, bypassing ColumnValues dispatch.
    pub async fn write_decimal(
        &mut self,
        column_index: usize,
        value: &crate::datatypes::decoder::DecimalParts,
    ) -> TdsResult<()> {
        let ctx = &self.column_contexts[column_index];
        TdsValueSerializer::serialize_decimal(self.packet_writer, value, ctx).await
    }

    /// Write pre-serialized TDS wire format bytes directly to the packet.
    ///
    /// This is a convenience method for writing raw TDS bytes that have been
    /// serialized externally (e.g., by Python code). It uses the internal
    /// TdsPacketWriter trait to write the bytes.
    ///
    /// # Safety
    ///
    /// Caller must ensure the bytes are valid TDS wire format for the expected
    /// column types, or SQL Server will reject the data.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Pre-serialized TDS wire format bytes
    ///
    /// # Errors
    ///
    /// Returns an error if network transmission fails.
    pub async fn write_raw_bytes(&mut self, bytes: &[u8]) -> TdsResult<()> {
        self.packet_writer.write_async(bytes).await
    }

    /// Check if `byte_count` bytes fit in the current packet buffer.
    pub fn has_space(&self, byte_count: usize) -> bool {
        self.packet_writer.has_space(byte_count)
    }

    /// Write bytes directly into the packet buffer without overflow checks.
    /// Caller **must** call [`has_space`] first or [`flush_if_needed`] after.
    pub fn write_bytes_unchecked(&mut self, bytes: &[u8]) {
        self.packet_writer.write_unchecked(bytes);
    }

    /// Write a single byte directly into the packet buffer.
    pub fn write_byte_unchecked(&mut self, b: u8) {
        self.packet_writer.write_byte_unchecked(b);
    }

    /// Write a little-endian i32 directly into the packet buffer.
    pub fn write_i32_unchecked(&mut self, v: i32) {
        self.packet_writer.write_i32_unchecked(v);
    }

    /// Write a little-endian i64 directly into the packet buffer.
    pub fn write_i64_unchecked(&mut self, v: i64) {
        self.packet_writer.write_i64_unchecked(v);
    }

    /// Write a little-endian u16 directly into the packet buffer.
    pub fn write_u16_unchecked(&mut self, v: u16) {
        self.packet_writer.write_u16_unchecked(v);
    }

    /// Write a little-endian f64 directly into the packet buffer.
    pub fn write_f64_unchecked(&mut self, v: f64) {
        self.packet_writer.write_f64_unchecked(v);
    }

    /// Flush the packet buffer to the network if it has overflowed.
    /// Call after a batch of unchecked writes.
    pub async fn flush_if_needed(&mut self) -> TdsResult<()> {
        self.packet_writer.check_overflow().await
    }

    /// Current write position in the packet buffer. Use with
    /// [`write_u16_at_position`] to patch a length prefix after writing data.
    pub fn unchecked_position(&self) -> usize {
        self.packet_writer.unchecked_position()
    }

    /// Patch a u16 at a previously recorded buffer position.
    pub fn write_u16_at_position(&mut self, pos: usize, value: u16) {
        self.packet_writer.write_u16_at_position(pos, value);
    }

    /// Begin a new row (for zero-copy bulk load).
    /// Writes the ROW token.
    pub(crate) async fn begin_row(&mut self) -> TdsResult<()> {
        if !self.metadata_written {
            return Err(Error::ProtocolError(
                "Must call begin() before begin_row()".to_string(),
            ));
        }

        // Write ROW token
        self.packet_writer.write_byte_async(TOKEN_ROW).await?;

        Ok(())
    }

    /// End the current row (for zero-copy bulk load).
    /// Increments row counter.
    pub(crate) fn end_row(&mut self) {
        self.rows_written += 1;

        trace!(
            "StreamingBulkLoadWriter: Row {} written (zero-copy)",
            self.rows_written
        );
    }

    /// Get the number of columns in the metadata.
    pub(crate) fn column_count(&self) -> usize {
        self.column_metadata.len()
    }

    /// Write a single row using zero-copy BulkLoadRow trait.
    ///
    /// This method provides zero-copy bulk insert by allowing the row
    /// to serialize directly to the packet writer.
    ///
    /// # Arguments
    ///
    /// * `row` - Row implementing BulkLoadRow trait
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - `begin()` has not been called yet
    /// - Network errors occur during transmission
    /// - Type conversion errors occur
    /// - Row has different column count than the first row
    pub async fn write_row_zerocopy<R>(&mut self, row: &R) -> TdsResult<()>
    where
        R: BulkLoadRow,
    {
        if !self.metadata_written {
            return Err(Error::ProtocolError(
                "Must call begin() before write_row_zerocopy()".to_string(),
            ));
        }

        // Write ROW token
        self.packet_writer.write_byte_async(TOKEN_ROW).await?;

        // Let the row serialize itself
        let mut column_index = 0usize;
        row.write_to_packet(self, &mut column_index).await?;

        // First row: record its column count as authoritative
        if self.first_row_column_count.is_none() {
            self.first_row_column_count = Some(column_index);
            trace!(
                "StreamingBulkLoadWriter: First row establishes column count: {}",
                column_index
            );
        } else {
            // Subsequent rows: validate against first row's column count
            let expected_count = self.first_row_column_count.unwrap();
            if column_index != expected_count {
                return Err(Error::UsageError(format!(
                    "Row {} has {} columns, but first row had {} columns. All rows must have the same number of columns as the first row.",
                    self.rows_written + 1,
                    column_index,
                    expected_count
                )));
            }
        }

        // Also verify against metadata for safety (this catches issues with column mappings)
        if column_index != self.column_metadata.len() {
            return Err(Error::UsageError(format!(
                "Row {} wrote {} columns, but expected {} columns based on table metadata",
                self.rows_written + 1,
                column_index,
                self.column_metadata.len()
            )));
        }

        // Increment row counter
        self.rows_written += 1;

        trace!(
            "StreamingBulkLoadWriter: Row {} written (zero-copy)",
            self.rows_written
        );

        Ok(())
    }

    /// End streaming - write DONE token and finalize packet.
    ///
    /// This consumes the writer and returns the number of rows written.
    ///
    /// # Returns
    ///
    /// The number of rows successfully written to the stream.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Network errors occur during transmission
    pub async fn end(mut self) -> TdsResult<u64> {
        if !self.metadata_written {
            return Err(Error::ProtocolError(
                "Must call begin() before end()".to_string(),
            ));
        }

        // Write DONE token
        self.write_done_token_internal().await?;

        // Finalize packet
        self.packet_writer.finalize().await?;

        debug!(
            "StreamingBulkLoadWriter: Completed - {} rows written",
            self.rows_written
        );

        Ok(self.rows_written)
    }

    /// Internal method to write metadata.
    async fn write_metadata_internal(&mut self) -> TdsResult<()> {
        self.packet_writer
            .write_byte_async(TOKEN_COLMETADATA)
            .await?;

        // Column count (2 bytes)
        let column_count = self.column_metadata.len();
        self.packet_writer
            .write_u16_async(column_count as u16)
            .await?;

        // Write each column descriptor
        // Cache metadata length to avoid borrow conflicts
        let metadata_len = self.column_metadata.len();
        for i in 0..metadata_len {
            // Clone individual metadata item to avoid holding immutable borrow
            // This is acceptable since we only do it once during metadata phase
            let col_meta = self.column_metadata[i].clone();
            self.write_column_descriptor_internal(&col_meta).await?;
        }

        Ok(())
    }

    /// Internal method to write column descriptor.
    async fn write_column_descriptor_internal(
        &mut self,
        col_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<()> {
        // User type (4 bytes) - always 0 for standard types
        self.packet_writer.write_u32_async(0).await?;

        // Flags (2 bytes)
        let mut flags: u16 = 0x0008; // Updatability flag
        if col_meta.is_nullable {
            flags |= 0x0001; // Nullable
        }
        if col_meta.is_identity {
            flags |= 0x0010; // Identity
        }
        self.packet_writer.write_u16_async(flags).await?;

        // TDS type byte
        self.packet_writer
            .write_byte_async(col_meta.tds_type)
            .await?;

        // Type-specific info
        self.write_type_info_internal(col_meta).await?;

        // Column name (B_VARCHAR format)
        let name_utf16: Vec<u16> = col_meta.column_name.encode_utf16().collect();
        self.packet_writer
            .write_byte_async((name_utf16.len() & 0xFF) as u8)
            .await?;
        for c in name_utf16 {
            self.packet_writer.write_u16_async(c).await?;
        }

        Ok(())
    }

    /// Internal method to write type info.
    /// TODO: This encoding is same as what we during parameter type_info encoding. Consider refactoring to share code.
    async fn write_type_info_internal(
        &mut self,
        col_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<()> {
        match col_meta.tds_type {
            // DECIMAL/NUMERIC - precision and scale
            x if x == TdsDataType::Decimal as u8
                || x == TdsDataType::Numeric as u8
                || x == TdsDataType::DecimalN as u8
                || x == TdsDataType::NumericN as u8 => {
                self.packet_writer
                    .write_byte_async(col_meta.length as u8)
                    .await?;
                self.packet_writer
                    .write_byte_async(col_meta.precision)
                    .await?;
                self.packet_writer.write_byte_async(col_meta.scale).await?;
            }

            // Fixed-length types - NO type info needed
            x if x == TdsDataType::Int1 as u8       // TINYINT
                || x == TdsDataType::Bit as u8      // BIT
                || x == TdsDataType::Int2 as u8     // SMALLINT
                || x == TdsDataType::Int4 as u8     // INT
                || x == TdsDataType::DateTim4 as u8 // SMALLDATETIME
                || x == TdsDataType::Flt4 as u8     // REAL
                || x == TdsDataType::Money as u8    // MONEY
                || x == TdsDataType::DateTime as u8 // DATETIME
                || x == TdsDataType::Flt8 as u8     // FLOAT
                || x == TdsDataType::Int8 as u8     // BIGINT
            => {
                // These are fixed-length types, no additional type info
            }

            // INTN, FLTN, BITN, MONEYN, DATETIMEN - length byte
            x if x == TdsDataType::IntN as u8
                || x == TdsDataType::FltN as u8
                || x == TdsDataType::BitN as u8
                || x == TdsDataType::MoneyN as u8
                || x == TdsDataType::DateTimeN as u8 => {
                self.packet_writer
                    .write_byte_async(col_meta.length as u8)
                    .await?;
            }

            // VARCHAR/CHAR types - length + collation
            x if x == TdsDataType::VarChar as u8
                || x == TdsDataType::Char as u8
                || x == TdsDataType::BigVarChar as u8
                || x == TdsDataType::BigChar as u8 => {
                if col_meta.is_plp() {
                    self.packet_writer.write_u16_async(0xFFFF).await?;
                } else {
                    self.packet_writer
                        .write_u16_async(col_meta.length as u16)
                        .await?;
                }

                if let Some(collation) = col_meta.collation {
                    self.packet_writer.write_u32_async(collation.info).await?;
                    self.packet_writer
                        .write_byte_async(collation.sort_id)
                        .await?;
                } else {
                    self.packet_writer.write_u32_async(0x00000409).await?;
                    self.packet_writer.write_byte_async(0).await?;
                }
            }

            // NVARCHAR/NCHAR types - length + collation
            x if x == TdsDataType::NChar as u8
                || x == TdsDataType::NVarChar as u8 => {
                if col_meta.is_plp() {
                    self.packet_writer.write_u16_async(0xFFFF).await?;
                } else {
                    // TDS COLMETADATA MaxLength for NVARCHAR/NCHAR is in BYTES
                    // col_meta.length is already in bytes (e.g., 10 for NVARCHAR(5))
                    self.packet_writer
                        .write_u16_async(col_meta.length as u16)
                        .await?;
                }

                if let Some(collation) = col_meta.collation {
                    self.packet_writer.write_u32_async(collation.info).await?;
                    self.packet_writer
                        .write_byte_async(collation.sort_id)
                        .await?;
                } else {
                    // Use connection's default collation (matches .NET SqlBulkCopy behavior)
                    self.packet_writer
                        .write_u32_async(self.default_collation.info)
                        .await?;
                    self.packet_writer
                        .write_byte_async(self.default_collation.sort_id)
                        .await?;
                }
            }

            // TEXT/NTEXT/IMAGE (Legacy LOB types) - length (4 bytes) + collation (5 bytes for text types) + table parts (1 byte)
            x if x == TdsDataType::Text as u8
                || x == TdsDataType::NText as u8
                || x == TdsDataType::Image as u8 => {
                // Write length as 4-byte integer (max length for legacy LOB types)
                // For TEXT/NTEXT/IMAGE, use 0x7FFFFFFE (2147483646) as per TDS spec
                self.packet_writer.write_u32_async(0x7FFFFFFE).await?;

                // TEXT and NTEXT require collation, IMAGE does not
                if x == TdsDataType::Text as u8 || x == TdsDataType::NText as u8 {
                    if let Some(collation) = col_meta.collation {
                        self.packet_writer.write_u32_async(collation.info).await?;
                        self.packet_writer
                            .write_byte_async(collation.sort_id)
                            .await?;
                    } else {
                        // Use connection's default collation
                        self.packet_writer
                            .write_u32_async(self.default_collation.info)
                            .await?;
                        self.packet_writer
                            .write_byte_async(self.default_collation.sort_id)
                            .await?;
                    }
                }

                // For legacy LOB types, write table name
                let table_name_utf16: Vec<u16> = self.table_name.encode_utf16().collect();
                // Table name length as SHORT (2 bytes) - this is the character count
                self.packet_writer.write_u16_async(table_name_utf16.len() as u16).await?;
                // Table name as UTF-16 string
                for c in table_name_utf16 {
                    self.packet_writer.write_u16_async(c).await?;
                }
            }

            // VARBINARY/BINARY types - length
            x if x == TdsDataType::VarBinary as u8
                || x == TdsDataType::Binary as u8
                || x == TdsDataType::BigVarBinary as u8
                || x == TdsDataType::BigBinary as u8 => {
                if col_meta.is_plp() {
                    self.packet_writer.write_u16_async(0xFFFF).await?;
                } else {
                    self.packet_writer
                        .write_u16_async(col_meta.length as u16)
                        .await?;
                }
            }

            // XML - schema info (no schema support yet)
            x if x == TdsDataType::Xml as u8 => {
                self.packet_writer.write_byte_async(0).await?;
            }

            // JSON - schema info (similar to XML, no schema support yet)
            x if x == TdsDataType::Json as u8 => {
                self.packet_writer.write_byte_async(0).await?;
            }

            // Time types - scale only
            x if x == TdsDataType::TimeN as u8
                || x == TdsDataType::DateTime2N as u8
                || x == TdsDataType::DateTimeOffsetN as u8 => {
                trace!("Writing TIME type metadata: tds_type=0x{:02X}, length={}, scale={}", 
                       col_meta.tds_type, col_meta.length, col_meta.scale);
                self.packet_writer.write_byte_async(col_meta.scale).await?;
            }

            // DATE - no type info
            x if x == TdsDataType::DateN as u8 => {}

            // SQL_VARIANT - 4-byte max length
            x if x == TdsDataType::SsVariant as u8 => {
                self.packet_writer.write_u32_async(col_meta.length as u32).await?;
            }

            // UNIQUEIDENTIFIER (GUIDTYPE) - requires length byte (always 16)
            x if x == TdsDataType::Guid as u8 => {
                self.packet_writer.write_byte_async(16u8).await?;
            }

            // VECTOR type - USHORT length (total length) + SCALE (base type)
            x if x == TdsDataType::Vector as u8 => {
                // Length is the payload size in bytes (header + elements)
                self.packet_writer
                    .write_u16_async(col_meta.length as u16)
                    .await?;
                // SCALE stores base type (e.g., 0x00 for Float32)
                self.packet_writer
                    .write_byte_async(col_meta.scale)
                    .await?;
            }

            _ => {
                return Err(Error::ProtocolError(format!(
                    "Unsupported TDS type for bulk copy: 0x{:02X}",
                    col_meta.tds_type
                )));
            }
        }

        Ok(())
    }

    /// Internal method to write DONE token.
    async fn write_done_token_internal(&mut self) -> TdsResult<()> {
        self.packet_writer.write_byte_async(TOKEN_DONE).await?;
        self.packet_writer.write_u16_async(0x0000).await?; // Status
        self.packet_writer.write_u16_async(0x0000).await?; // CurCmd
        self.packet_writer.write_u32_async(0).await?; // Row count (client sends 4 bytes)

        Ok(())
    }
}

/// Helper function to build the INSERT BULK SQL command.
///
/// This is used by both `BulkLoadMessage` and streaming bulk copy operations.
///
/// # Arguments
///
/// * `table_name` - Destination table name
/// * `column_metadata` - Column metadata for the bulk load
/// * `options` - Bulk copy options
///
/// # Returns
///
/// The INSERT BULK SQL command string
pub(crate) fn build_insert_bulk_command(
    table_name: &str,
    column_metadata: &[BulkCopyColumnMetadata],
    options: &BulkCopyOptions,
) -> String {
    let mut command = format!("INSERT BULK {table_name} (");

    for (i, col_meta) in column_metadata.iter().enumerate() {
        if i > 0 {
            command.push_str(", ");
        }

        // Column name
        command.push_str(&format!("[{}] ", col_meta.column_name));

        // Type definition
        let type_def = col_meta.get_sql_type_definition();
        command.push_str(&type_def);

        // Add COLLATE clause if the column needs collation and has a collation name
        if let (true, Some(collation_name)) = (col_meta.needs_collation(), &col_meta.collation_name)
        {
            command.push_str(&format!(" COLLATE {}", collation_name));
        }
    }

    command.push(')');

    // Add WITH clause for options
    let mut option_list = Vec::new();
    if options.keep_nulls {
        option_list.push("KEEP_NULLS");
    }
    if options.table_lock {
        option_list.push("TABLOCK");
    }
    if options.check_constraints {
        option_list.push("CHECK_CONSTRAINTS");
    }
    if options.fire_triggers {
        option_list.push("FIRE_TRIGGERS");
    }
    // Note: KEEP_IDENTITY is NOT an INSERT BULK hint (unlike BULK INSERT).
    // Identity preservation is controlled through the TDS column metadata flags
    // (0x0010 identity flag) which is set when is_identity=true on column metadata.
    // The keep_identity option controls whether we include identity columns in
    // the bulk copy operation and send their values.

    if !option_list.is_empty() {
        command.push_str(" WITH (");
        command.push_str(&option_list.join(", "));
        command.push(')');
    }

    command
}

// Include additional unit tests from separate test file
#[cfg(test)]
#[path = "bulk_load_tests.rs"]
mod bulk_load_tests;
