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
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::token::tokens::SqlCollation;
use tracing::{debug, trace};

use crate::datatypes::bulk_copy_metadata::BulkCopyColumnEncryption;
use crate::datatypes::sqldatatypes::{TypeInfo, TypeInfoVariant, VariableLengthTypes};
use crate::query::metadata::CekTableEntry;
use crate::security::encryption::encrypt_cell_value;

// TDS Token types
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ROW: u8 = 0xD1;
const TOKEN_DONE: u8 = 0xFD;

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

    /// Whether Always Encrypted was negotiated for the connection. When set, the
    /// BCP COLMETADATA carries a CEK table and per-encrypted-column crypto
    /// metadata, mirroring what the server sends on the read side.
    column_encryption_enabled: bool,

    /// The deduplicated CEK table emitted in the COLMETADATA, built from the
    /// per-column encryption material during `begin()`. Column crypto metadata
    /// references entries here by ordinal.
    emitted_cek_table: Vec<CekTableEntry>,

    /// Per-column plaintext (decrypted) column encryption keys, aligned with
    /// `column_metadata`. `None` for plaintext columns. Populated by the caller
    /// before `begin()` when column encryption is enabled; used to encrypt cell
    /// values on the write path.
    plaintext_ceks: Vec<Option<std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>>>,

    /// When set, encrypted destination columns receive their values verbatim as
    /// varbinary ciphertext instead of being encrypted with the column's key
    /// (the .NET `AllowEncryptedValueModifications` behavior). No plaintext CEK
    /// is required for the encrypted columns in this mode.
    allow_encrypted_value_modifications: bool,
}

impl<'a> StreamingBulkLoadWriter<'a> {
    /// Create a new streaming bulk load writer.
    ///
    /// # Arguments
    ///
    /// * `packet_writer` - TDS packet writer
    /// * `table_name` - Destination table name                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               
    /// * `column_metadata` - Column metadata for the bulk load
    /// * `default_collation` - Connection's default collation (used when column metadata doesn't specify collation)
    pub fn new(
        packet_writer: &'a mut PacketWriter<'a>,
        table_name: String,
        column_metadata: Vec<BulkCopyColumnMetadata>,
        default_collation: SqlCollation,
    ) -> Self {
        Self {
            packet_writer,
            table_name,
            column_metadata,
            default_collation,
            metadata_written: false,
            rows_written: 0,
            column_contexts: Vec::new(),  // Will be populated in begin()
            first_row_column_count: None, // Will be set when first row is written
            column_encryption_enabled: false,
            emitted_cek_table: Vec::new(),
            plaintext_ceks: Vec::new(),
            allow_encrypted_value_modifications: false,
        }
    }

    /// Enable Always Encrypted serialization for this bulk load.
    ///
    /// When enabled, `begin()` emits the CEK table after the column count and
    /// per-encrypted-column crypto metadata in each column descriptor, matching
    /// the COLMETADATA layout the server uses on the read side. The caller must
    /// only enable this when column encryption has been negotiated for the
    /// connection.
    pub(crate) fn set_column_encryption_enabled(&mut self, enabled: bool) {
        self.column_encryption_enabled = enabled;
    }

    /// Provide the per-column plaintext (decrypted) column encryption keys,
    /// aligned with the column metadata. Entries are `None` for plaintext
    /// columns. Must be called before `begin()` when column encryption is
    /// enabled and any column is encrypted.
    pub(crate) fn set_plaintext_ceks(
        &mut self,
        ceks: Vec<Option<std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>>>,
    ) {
        self.plaintext_ceks = ceks;
    }

    /// Enable ciphertext passthrough for encrypted columns (the
    /// `AllowEncryptedValueModifications` behavior). When set, values for
    /// encrypted columns are emitted verbatim as varbinary ciphertext rather
    /// than encrypted, so the caller need not supply plaintext CEKs for those
    /// columns. Requires
    /// [`set_column_encryption_enabled`](Self::set_column_encryption_enabled) so
    /// the encrypted COLMETADATA is still emitted.
    pub(crate) fn set_allow_encrypted_value_modifications(&mut self, enabled: bool) {
        self.allow_encrypted_value_modifications = enabled;
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
        // Ciphertext passthrough (AllowEncryptedValueModifications): for an
        // encrypted column, serialize the caller-supplied ciphertext verbatim —
        // no plaintext CEK, no re-encryption, and no clone of the (potentially
        // large) ciphertext buffer. The value must already be varbinary
        // ciphertext (or NULL); anything else is a usage error rather than
        // silently storing an un-decryptable value.
        if self.allow_encrypted_value_modifications
            && self
                .column_metadata
                .get(column_index)
                .is_some_and(|col| col.encryption.is_some())
        {
            if !matches!(value, ColumnValues::Bytes(_) | ColumnValues::Null) {
                return Err(Error::UsageError(format!(
                    "allow_encrypted_value_modifications requires already-encrypted varbinary \
                     ciphertext for encrypted column '{}', but got {value:?}",
                    self.column_metadata[column_index].column_name
                )));
            }
            let ctx = self.column_contexts.get(column_index).cloned().ok_or_else(|| {
                Error::UsageError(format!(
                    "Column index {} out of bounds, expected {} columns based on table metadata.",
                    column_index,
                    self.column_contexts.len()
                ))
            })?;
            TdsValueSerializer::serialize_value(self.packet_writer, value, &ctx).await?;
            return Ok(());
        }

        // Always Encrypted: encrypt the plaintext cell value and emit the
        // ciphertext as a varbinary, matching the encrypted COLMETADATA wire
        // type. NULL values stay NULL (no ciphertext).
        if self.column_encryption_enabled
            && let Some(encrypted) = self.try_encrypt_cell(column_index, value)?
        {
            let ctx = self.column_contexts.get(column_index).cloned().ok_or_else(|| {
                Error::UsageError(format!(
                    "Column index {} out of bounds, expected {} columns based on table metadata.",
                    column_index,
                    self.column_contexts.len()
                ))
            })?;
            TdsValueSerializer::serialize_value(self.packet_writer, &encrypted, &ctx).await?;
            return Ok(());
        }

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

    /// Encrypts a cell value for an encrypted column, returning the ciphertext
    /// wrapped as `ColumnValues::Bytes` (or `ColumnValues::Null` for NULL
    /// input). Returns `Ok(None)` when the column is not encrypted, so the
    /// caller falls through to the plaintext serialization path.
    fn try_encrypt_cell(
        &self,
        column_index: usize,
        value: &ColumnValues,
    ) -> TdsResult<Option<ColumnValues>> {
        let Some(col_meta) = self.column_metadata.get(column_index) else {
            return Ok(None);
        };
        let Some(enc) = &col_meta.encryption else {
            return Ok(None);
        };

        let cek = self
            .plaintext_ceks
            .get(column_index)
            .and_then(|c| c.clone())
            .ok_or_else(|| {
                Error::ColumnEncryptionError(format!(
                    "Missing plaintext column encryption key for encrypted column '{}'",
                    col_meta.column_name
                ))
            })?;

        let crypto = &enc.crypto_metadata;
        let ciphertext = encrypt_cell_value(
            value,
            &cek,
            crypto.cipher_algorithm_id,
            crypto.encryption_type,
            crypto.normalization_rule_version,
        )?;

        Ok(Some(match ciphertext {
            Some(bytes) => ColumnValues::Bytes(bytes),
            None => ColumnValues::Null,
        }))
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
            let expected_count = self.first_row_column_count.ok_or_else(|| {
                Error::ImplementationError(
                    "First row column count is missing after initial row write".to_string(),
                )
            })?;
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

        // Always Encrypted: when column encryption is negotiated, the CEK table
        // is emitted right after the column count and before any column
        // descriptors (mirroring the read-side COLMETADATA layout). It is empty
        // when none of the columns are encrypted.
        if self.column_encryption_enabled {
            self.emitted_cek_table = self.collect_cek_table();
            self.write_cek_table().await?;
        }

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
        if self.column_encryption_enabled && col_meta.encryption.is_some() {
            flags |= 0x0800; // fEncrypted
        }
        self.packet_writer.write_u16_async(flags).await?;

        // TDS type byte
        self.packet_writer
            .write_byte_async(col_meta.tds_type)
            .await?;

        // Type-specific info
        self.write_type_info_internal(col_meta).await?;

        // Always Encrypted: encrypted columns carry a CryptoMetadata blob after
        // the (ciphertext) TYPE_INFO and before the column name.
        if self.column_encryption_enabled
            && let Some(enc) = &col_meta.encryption
        {
            let enc = enc.clone();
            self.write_crypto_metadata_colmetadata(&enc).await?;
        }

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

    /// Builds the deduplicated CEK table to emit in the COLMETADATA, collecting
    /// the distinct CEK entries referenced by the encrypted columns. Two entries
    /// are considered the same key when their identity tuple (database id, CEK
    /// id, version, metadata version) matches.
    fn collect_cek_table(&self) -> Vec<CekTableEntry> {
        let mut table: Vec<CekTableEntry> = Vec::new();
        for col in &self.column_metadata {
            if let Some(enc) = &col.encryption {
                let entry = &enc.cek_entry;
                if !table.iter().any(|e| cek_entry_matches(e, entry)) {
                    table.push(entry.clone());
                }
            }
        }
        table
    }

    /// Returns the ordinal of `entry` within the emitted CEK table.
    ///
    /// Errors if the entry is not present. `collect_cek_table` guarantees every
    /// referenced entry is emitted, so a miss would indicate a bug (or a future
    /// refactor) that must fail loudly rather than silently emit ordinal `0` —
    /// i.e. tell the server the value was encrypted under the wrong key.
    fn cek_table_ordinal_for(&self, entry: &CekTableEntry) -> TdsResult<u16> {
        self.emitted_cek_table
            .iter()
            .position(|e| cek_entry_matches(e, entry))
            .map(|pos| pos as u16)
            .ok_or_else(|| {
                crate::error::Error::ColumnEncryptionError(format!(
                    "bulk copy CEK table has no entry for column key \
                     (database_id={}, cek_id={}, cek_version={})",
                    entry.database_id, entry.cek_id, entry.cek_version
                ))
            })
    }

    /// Writes the CEK table: a `u16` entry count followed by each entry.
    async fn write_cek_table(&mut self) -> TdsResult<()> {
        let table = std::mem::take(&mut self.emitted_cek_table);
        self.packet_writer
            .write_u16_async(table.len() as u16)
            .await?;
        for entry in &table {
            self.write_cek_table_entry(entry).await?;
        }
        self.emitted_cek_table = table;
        Ok(())
    }

    /// Writes a single CEK table entry for bulk copy.
    ///
    /// Unlike the server-sent CEK table (which carries the encrypted CEK
    /// values, key-store names and paths), the client-sent bulk-copy CEK table
    /// only needs to identify each key by `(database_id, cek_id, cek_version,
    /// cek_md_version)`; the server already holds the encrypted key material and
    /// reconciles by those identifiers. Accordingly the encrypted-CEK-value
    /// count is written as `0`, matching .NET's `WriteEncryptionEntries`.
    async fn write_cek_table_entry(&mut self, entry: &CekTableEntry) -> TdsResult<()> {
        self.packet_writer
            .write_i32_async(entry.database_id)
            .await?;
        self.packet_writer.write_i32_async(entry.cek_id).await?;
        self.packet_writer
            .write_i32_async(entry.cek_version)
            .await?;
        self.packet_writer
            .write_async(&entry.cek_md_version)
            .await?;
        // Encrypted CEK value count: always 0 for client-sent CEK tables.
        self.packet_writer.write_byte_async(0x00).await?;
        Ok(())
    }

    /// Writes the per-column CryptoMetadata that follows the ciphertext TYPE_INFO
    /// of an encrypted column (mirrors `parse_crypto_metadata` with a CEK table
    /// present).
    async fn write_crypto_metadata_colmetadata(
        &mut self,
        enc: &BulkCopyColumnEncryption,
    ) -> TdsResult<()> {
        let crypto = &enc.crypto_metadata;

        // CekTableOrdinal (u16) - present because the BCP COLMETADATA carries a
        // CEK table.
        let ordinal = self.cek_table_ordinal_for(&enc.cek_entry)?;
        self.packet_writer.write_u16_async(ordinal).await?;

        // User type of the base column (4 bytes); 0 for built-in types.
        self.packet_writer.write_u32_async(0).await?;

        // Base (plaintext) TYPE_INFO.
        let base_data_type = crypto.base_data_type;
        let base_type_info = crypto.base_type_info.clone();
        self.write_base_type_info(base_data_type, &base_type_info)
            .await?;

        // CipherAlgorithmId (+ optional custom algorithm name).
        self.packet_writer
            .write_byte_async(crypto.cipher_algorithm_id)
            .await?;
        if crypto.cipher_algorithm_id == 0x00 {
            let name = crypto.cipher_algorithm_name.clone().unwrap_or_default();
            self.write_b_varchar(&name).await?;
        }

        // EncryptionType + NormalizationRuleVersion.
        self.packet_writer
            .write_byte_async(crypto.encryption_type)
            .await?;
        self.packet_writer
            .write_byte_async(crypto.normalization_rule_version)
            .await?;
        Ok(())
    }

    /// Writes a base TYPE_INFO (type byte + type-specific metadata) for the
    /// plaintext type of an encrypted column. This is the inverse of
    /// `read_type_info` for the data types Always Encrypted supports.
    async fn write_base_type_info(
        &mut self,
        base_data_type: TdsDataType,
        info: &TypeInfo,
    ) -> TdsResult<()> {
        self.packet_writer
            .write_byte_async(base_data_type as u8)
            .await?;

        match &info.type_info_variant {
            // Fixed-length types carry no extra metadata.
            TypeInfoVariant::FixedLen(_) => {}

            // Variable-length non-string types: a length declarator whose width
            // depends on the type, except DATE which has none.
            TypeInfoVariant::VarLen(t, length) => match t {
                VariableLengthTypes::DateN => {}
                _ => self.write_type_length(t, *length).await?,
            },

            // Scale-bearing temporal types: a single scale byte.
            TypeInfoVariant::VarLenScale(_, scale) => {
                self.packet_writer.write_byte_async(*scale).await?;
            }

            // Decimal/numeric: length declarator + precision + scale.
            TypeInfoVariant::VarLenPrecisionScale(t, length, precision, scale) => {
                self.write_type_length(t, *length).await?;
                self.packet_writer.write_byte_async(*precision).await?;
                self.packet_writer.write_byte_async(*scale).await?;
            }

            // Character types: length declarator + 5-byte collation.
            TypeInfoVariant::VarLenString(t, length, collation) => {
                self.write_type_length(t, *length).await?;
                self.write_collation_bytes(collation).await?;
            }

            // (max)/PLP base types are not valid for Always Encrypted columns.
            TypeInfoVariant::PartialLen(..) => {
                return Err(Error::ColumnEncryptionError(format!(
                    "Unsupported PLP base type {base_data_type:?} for an encrypted column"
                )));
            }
        }
        Ok(())
    }

    /// Writes a variable-length type's length declarator using the byte width
    /// the type uses on the wire (1, 2, or 4 bytes).
    async fn write_type_length(&mut self, t: &VariableLengthTypes, length: usize) -> TdsResult<()> {
        match t.get_len_byte_count() {
            1 => self.packet_writer.write_byte_async(length as u8).await?,
            2 => self.packet_writer.write_u16_async(length as u16).await?,
            4 => self.packet_writer.write_u32_async(length as u32).await?,
            other => {
                return Err(Error::ProtocolError(format!(
                    "Unexpected length byte count {other} for base type {t:?}"
                )));
            }
        }
        Ok(())
    }

    /// Writes a 5-byte collation block (4-byte info + sort id), or five zero
    /// bytes when no collation is present.
    async fn write_collation_bytes(&mut self, collation: &Option<SqlCollation>) -> TdsResult<()> {
        match collation {
            Some(c) => {
                self.packet_writer.write_u32_async(c.info).await?;
                self.packet_writer.write_byte_async(c.sort_id).await?;
            }
            None => {
                self.packet_writer.write_async(&[0u8; 5]).await?;
            }
        }
        Ok(())
    }

    /// Writes a `B_VARCHAR` (1-byte UTF-16 char count followed by the string).
    async fn write_b_varchar(&mut self, value: &str) -> TdsResult<()> {
        let utf16: Vec<u16> = value.encode_utf16().collect();
        self.packet_writer
            .write_byte_async(utf16.len() as u8)
            .await?;
        for c in utf16 {
            self.packet_writer.write_u16_async(c).await?;
        }
        Ok(())
    }
}

/// Returns whether two CEK table entries describe the same key (matching
/// identity: database id, CEK id, version, and metadata version).
fn cek_entry_matches(a: &CekTableEntry, b: &CekTableEntry) -> bool {
    a.database_id == b.database_id
        && a.cek_id == b.cek_id
        && a.cek_version == b.cek_version
        && a.cek_md_version == b.cek_md_version
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
) -> crate::core::TdsResult<String> {
    let mut command = format!("INSERT BULK {table_name} (");

    for (i, col_meta) in column_metadata.iter().enumerate() {
        if i > 0 {
            command.push_str(", ");
        }

        // Column name
        command.push_str(&format!("[{}] ", col_meta.column_name));

        // Type definition
        let type_def = col_meta.get_sql_type_definition()?;
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

    Ok(command)
}

// Include additional unit tests from separate test file
#[cfg(test)]
#[path = "bulk_load_tests.rs"]
mod bulk_load_tests;

/// Always Encrypted COLMETADATA serialization tests.
///
/// These round-trip the encrypted COLMETADATA the bulk-load writer emits back
/// through the production `ColMetadataTokenParser`, verifying the CEK table and
/// per-column crypto metadata are byte-compatible with the read side.
#[cfg(test)]
mod ae_colmetadata_tests {
    use super::*;
    use crate::connection::transport::network_transport::TransportSslHandler;
    use crate::core::NegotiatedEncryptionSetting;
    use crate::datatypes::bulk_copy_metadata::{
        BulkCopyColumnEncryption, BulkCopyColumnMetadata, SqlDbType, TypeLength,
    };
    use crate::datatypes::sqldatatypes::TdsDataType;
    use crate::io::packet_reader::TdsPacketReader;
    use crate::io::reader_writer::NetworkWriter;
    use crate::io::token_stream::ParserContext;
    use crate::message::messages::PacketType;
    use crate::query::metadata::{CekTableEntry, CryptoMetadata, EncryptedCekValue};
    use crate::token::parsers::common::test_utils::MockReader;
    use crate::token::parsers::{ColMetadataTokenParser, TokenParser};
    use crate::token::tokens::Tokens;
    use async_trait::async_trait;

    /// Captures all bytes the packet writer flushes, without touching a network.
    struct CapturingWriter {
        buffer: Vec<u8>,
    }

    #[async_trait]
    impl TransportSslHandler for CapturingWriter {
        async fn enable_ssl(&mut self) -> TdsResult<()> {
            Ok(())
        }
        async fn disable_ssl(&mut self) -> TdsResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl NetworkWriter for CapturingWriter {
        async fn send(&mut self, data: &[u8]) -> TdsResult<()> {
            self.buffer.extend_from_slice(data);
            Ok(())
        }
        fn packet_size(&self) -> u32 {
            8192
        }
        fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
            NegotiatedEncryptionSetting::NoEncryption
        }
    }

    fn deterministic_int_column() -> BulkCopyColumnMetadata {
        // Wire type for an encrypted column is always varbinary; the plaintext
        // type (int here) lives in the crypto metadata. The varbinary capacity
        // must hold the AEAD ciphertext, so the destination column is sized
        // accordingly (here varbinary(256)).
        let mut col = BulkCopyColumnMetadata::new(
            "secret",
            SqlDbType::VarBinary,
            TdsDataType::BigVarBinary as u8,
        )
        .with_length(256, TypeLength::Variable(256));
        col.encryption = Some(BulkCopyColumnEncryption {
            crypto_metadata: CryptoMetadata {
                cek_table_ordinal: 0,
                base_data_type: TdsDataType::Int4,
                base_type_info: crate::datatypes::sqldatatypes::TypeInfo::fixed_len(
                    TdsDataType::Int4,
                )
                .unwrap(),
                cipher_algorithm_id: 2,
                cipher_algorithm_name: None,
                encryption_type: 1,
                normalization_rule_version: 1,
            },
            cek_entry: CekTableEntry {
                database_id: 5,
                cek_id: 7,
                cek_version: 1,
                cek_md_version: [1, 2, 3, 4, 5, 6, 7, 8],
                encrypted_cek_values: vec![EncryptedCekValue {
                    encrypted_key: vec![0xDE, 0xAD, 0xBE, 0xEF],
                    key_store_name: "AKV".to_string(),
                    key_path: "https://vault/key".to_string(),
                    algorithm_name: "RSA_OAEP".to_string(),
                }],
            },
        });
        col
    }

    fn plain_int_column() -> BulkCopyColumnMetadata {
        BulkCopyColumnMetadata::new("id", SqlDbType::Int, TdsDataType::Int4 as u8)
            .with_length(4, TypeLength::Fixed(4))
    }

    /// Serializes the COLMETADATA for the given columns (with column encryption
    /// enabled) and returns the COLMETADATA body bytes (after the packet header
    /// and the `0x81` token byte).
    async fn serialize_colmetadata_body(cols: Vec<BulkCopyColumnMetadata>) -> Vec<u8> {
        let mut net = CapturingWriter { buffer: Vec::new() };
        {
            let mut packet_writer = PacketWriter::new(PacketType::BulkLoad, &mut net, None, None);
            let mut writer = StreamingBulkLoadWriter::new(
                &mut packet_writer,
                "T".to_string(),
                cols,
                SqlCollation::default(),
            );
            writer.set_column_encryption_enabled(true);
            writer.begin().await.unwrap();
            // end() writes a trailing DONE token and finalizes the packet; the
            // parser stops after the declared columns, so the trailing bytes are
            // harmless to the round-trip assertion.
            let _ = writer.end().await.unwrap();
        }
        // Strip the 8-byte TDS packet header and the COLMETADATA token byte.
        assert!(net.buffer.len() > 9, "expected a non-empty packet");
        assert_eq!(
            net.buffer[8], TOKEN_COLMETADATA,
            "expected COLMETADATA token"
        );
        net.buffer[9..].to_vec()
    }

    #[tokio::test]
    async fn encrypted_column_colmetadata_roundtrips_through_parser() {
        let body = serialize_colmetadata_body(vec![deterministic_int_column()]).await;

        let mut reader = MockReader::new(body);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);
        let token = parser.parse(&mut reader, &context).await.unwrap();

        match token {
            Tokens::ColMetadata(t) => {
                assert_eq!(t.column_count, 1);

                // CEK table round-trips: the client-written entry identifies
                // the key by (database_id, cek_id, cek_version, cek_md_version)
                // and carries no encrypted CEK values (count 0), matching .NET.
                assert_eq!(t.cek_table.len(), 1);
                let entry = &t.cek_table[0];
                assert_eq!(entry.database_id, 5);
                assert_eq!(entry.cek_id, 7);
                assert_eq!(entry.cek_version, 1);
                assert_eq!(entry.cek_md_version, [1, 2, 3, 4, 5, 6, 7, 8]);
                assert!(entry.encrypted_cek_values.is_empty());

                // Encrypted column + crypto metadata round-trip exactly.
                let col = &t.columns[0];
                assert_eq!(col.column_name, "secret");
                assert!(col.is_encrypted());
                let crypto = col.crypto_metadata.as_ref().expect("crypto metadata");
                assert_eq!(crypto.cek_table_ordinal, 0);
                assert_eq!(crypto.base_data_type, TdsDataType::Int4);
                assert_eq!(crypto.cipher_algorithm_id, 2);
                assert!(crypto.cipher_algorithm_name.is_none());
                assert_eq!(crypto.encryption_type, 1);
                assert_eq!(crypto.normalization_rule_version, 1);
            }
            other => panic!("expected ColMetadata token, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mixed_plain_and_encrypted_columns_roundtrip() {
        let body =
            serialize_colmetadata_body(vec![plain_int_column(), deterministic_int_column()]).await;

        let mut reader = MockReader::new(body);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);
        let token = parser.parse(&mut reader, &context).await.unwrap();

        match token {
            Tokens::ColMetadata(t) => {
                assert_eq!(t.column_count, 2);
                assert_eq!(t.cek_table.len(), 1);

                // First column is plaintext.
                assert_eq!(t.columns[0].column_name, "id");
                assert!(!t.columns[0].is_encrypted());
                assert!(t.columns[0].crypto_metadata.is_none());

                // Second column is encrypted and references the single CEK entry.
                assert_eq!(t.columns[1].column_name, "secret");
                assert!(t.columns[1].is_encrypted());
                let crypto = t.columns[1]
                    .crypto_metadata
                    .as_ref()
                    .expect("crypto metadata");
                assert_eq!(crypto.cek_table_ordinal, 0);
                assert_eq!(crypto.base_data_type, TdsDataType::Int4);
            }
            other => panic!("expected ColMetadata token, got {other:?}"),
        }
    }

    /// A 32-byte plaintext CEK used for the row-encryption test.
    const TEST_CEK: [u8; 32] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
        0x1F, 0x20,
    ];

    #[tokio::test]
    async fn encrypted_row_value_is_ciphertext_varbinary() {
        use std::sync::Arc;

        let value = ColumnValues::Int(0x1234_5678);

        // Deterministic encryption produces a stable blob for the same input, so
        // we can compute the expected ciphertext independently.
        let expected_blob = encrypt_cell_value(&value, &TEST_CEK, 2, 1, 1)
            .unwrap()
            .expect("non-null ciphertext");

        let mut net = CapturingWriter { buffer: Vec::new() };
        {
            let mut packet_writer = PacketWriter::new(PacketType::BulkLoad, &mut net, None, None);
            let mut writer = StreamingBulkLoadWriter::new(
                &mut packet_writer,
                "T".to_string(),
                vec![deterministic_int_column()],
                SqlCollation::default(),
            );
            writer.set_column_encryption_enabled(true);
            writer.set_plaintext_ceks(vec![Some(Arc::new(zeroize::Zeroizing::new(
                TEST_CEK.to_vec(),
            )))]);
            writer.begin().await.unwrap();
            writer.write_column_value(0, &value).await.unwrap();
            let _ = writer.end().await.unwrap();
        }

        // Strip the packet header + COLMETADATA token byte, parse the metadata,
        // then read the encrypted column value (a varbinary) that follows.
        let body = net.buffer[9..].to_vec();
        let mut reader = MockReader::new(body);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);
        let _ = parser.parse(&mut reader, &context).await.unwrap();

        // The encrypted value is serialized as a non-PLP varbinary: a u16 byte
        // count followed by the AEAD ciphertext blob.
        let len = reader.read_uint16().await.unwrap() as usize;
        assert_eq!(len, expected_blob.len());
        let mut blob = vec![0u8; len];
        reader.read_bytes(&mut blob).await.unwrap();
        assert_eq!(blob, expected_blob);
    }

    #[tokio::test]
    async fn encrypted_null_row_value_stays_null() {
        use std::sync::Arc;

        let mut net = CapturingWriter { buffer: Vec::new() };
        {
            let mut packet_writer = PacketWriter::new(PacketType::BulkLoad, &mut net, None, None);
            let mut writer = StreamingBulkLoadWriter::new(
                &mut packet_writer,
                "T".to_string(),
                vec![deterministic_int_column()],
                SqlCollation::default(),
            );
            writer.set_column_encryption_enabled(true);
            writer.set_plaintext_ceks(vec![Some(Arc::new(zeroize::Zeroizing::new(
                TEST_CEK.to_vec(),
            )))]);
            writer.begin().await.unwrap();
            writer
                .write_column_value(0, &ColumnValues::Null)
                .await
                .unwrap();
            let _ = writer.end().await.unwrap();
        }

        let body = net.buffer[9..].to_vec();
        let mut reader = MockReader::new(body);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);
        let _ = parser.parse(&mut reader, &context).await.unwrap();

        // A NULL varbinary is the 0xFFFF length sentinel.
        let len = reader.read_uint16().await.unwrap();
        assert_eq!(len, 0xFFFF);
    }

    #[tokio::test]
    async fn passthrough_emits_ciphertext_verbatim_without_cek() {
        // With AllowEncryptedValueModifications the caller-supplied ciphertext is
        // written verbatim as a varbinary, and no plaintext CEK is required.
        let ciphertext = vec![0x10u8, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90];

        let mut net = CapturingWriter { buffer: Vec::new() };
        {
            let mut packet_writer = PacketWriter::new(PacketType::BulkLoad, &mut net, None, None);
            let mut writer = StreamingBulkLoadWriter::new(
                &mut packet_writer,
                "T".to_string(),
                vec![deterministic_int_column()],
                SqlCollation::default(),
            );
            writer.set_column_encryption_enabled(true);
            writer.set_allow_encrypted_value_modifications(true);
            // Intentionally no set_plaintext_ceks: passthrough must not need it.
            writer.begin().await.unwrap();
            writer
                .write_column_value(0, &ColumnValues::Bytes(ciphertext.clone()))
                .await
                .unwrap();
            let _ = writer.end().await.unwrap();
        }

        let body = net.buffer[9..].to_vec();
        let mut reader = MockReader::new(body);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);
        let _ = parser.parse(&mut reader, &context).await.unwrap();

        let len = reader.read_uint16().await.unwrap() as usize;
        assert_eq!(len, ciphertext.len(), "verbatim ciphertext length");
        let mut blob = vec![0u8; len];
        reader.read_bytes(&mut blob).await.unwrap();
        assert_eq!(blob, ciphertext, "ciphertext must be sent unchanged");
    }

    #[tokio::test]
    async fn passthrough_null_serializes_as_null() {
        let mut net = CapturingWriter { buffer: Vec::new() };
        {
            let mut packet_writer = PacketWriter::new(PacketType::BulkLoad, &mut net, None, None);
            let mut writer = StreamingBulkLoadWriter::new(
                &mut packet_writer,
                "T".to_string(),
                vec![deterministic_int_column()],
                SqlCollation::default(),
            );
            writer.set_column_encryption_enabled(true);
            writer.set_allow_encrypted_value_modifications(true);
            writer.begin().await.unwrap();
            writer
                .write_column_value(0, &ColumnValues::Null)
                .await
                .unwrap();
            let _ = writer.end().await.unwrap();
        }

        let body = net.buffer[9..].to_vec();
        let mut reader = MockReader::new(body);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);
        let _ = parser.parse(&mut reader, &context).await.unwrap();

        // A NULL varbinary is the 0xFFFF length sentinel.
        let len = reader.read_uint16().await.unwrap();
        assert_eq!(len, 0xFFFF);
    }

    #[tokio::test]
    async fn passthrough_rejects_non_ciphertext_value() {
        // A plaintext typed value under passthrough is a usage error rather than
        // silently storing an un-decryptable value in the encrypted column.
        let mut net = CapturingWriter { buffer: Vec::new() };
        let mut packet_writer = PacketWriter::new(PacketType::BulkLoad, &mut net, None, None);
        let mut writer = StreamingBulkLoadWriter::new(
            &mut packet_writer,
            "T".to_string(),
            vec![deterministic_int_column()],
            SqlCollation::default(),
        );
        writer.set_column_encryption_enabled(true);
        writer.set_allow_encrypted_value_modifications(true);
        writer.begin().await.unwrap();

        let err = writer
            .write_column_value(0, &ColumnValues::Int(42))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("ciphertext"),
            "expected a ciphertext usage error, got: {err}"
        );
    }
}
