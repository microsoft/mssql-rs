// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt;
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::Arc;
use std::{fmt::Debug, io::Error, vec};

use super::{
    sql_string::{SqlString, get_encoding_type},
    sqldatatypes::{PartialLengthType, TdsDataType, TypeInfoVariant},
};
use crate::datatypes::sqldatatypes::TypeInfo;
use crate::security::cell_decryptor::CellDecryptor;
use crate::{
    core::TdsResult,
    datatypes::{sql_json::SqlJson, sql_string::EncodingType, sqldatatypes::FixedLengthTypes},
};
use crate::{
    datatypes::column_values::{
        ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
        SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
    },
    io::packet_reader::TdsPacketReader,
};
use crate::{query::metadata::ColumnMetadata, token::tokens::SqlCollation};

use super::row_writer::{RowWriter, ValueKind, write_column_value};

// Avoid constructing a read future when the complete scalar is already buffered.
// Probe misses consume nothing; the async method remains the authoritative refill path.
macro_rules! read_sync_first {
    ($reader:expr, try_read_byte, read_byte) => {
        read_sync_first!(@pair $reader, try_read_byte, read_byte)
    };
    ($reader:expr, try_read_int16, read_int16) => {
        read_sync_first!(@pair $reader, try_read_int16, read_int16)
    };
    ($reader:expr, try_read_uint16, read_uint16) => {
        read_sync_first!(@pair $reader, try_read_uint16, read_uint16)
    };
    ($reader:expr, try_read_uint24, read_uint24) => {
        read_sync_first!(@pair $reader, try_read_uint24, read_uint24)
    };
    ($reader:expr, try_read_int32, read_int32) => {
        read_sync_first!(@pair $reader, try_read_int32, read_int32)
    };
    ($reader:expr, try_read_uint32, read_uint32) => {
        read_sync_first!(@pair $reader, try_read_uint32, read_uint32)
    };
    ($reader:expr, try_read_uint40, read_uint40) => {
        read_sync_first!(@pair $reader, try_read_uint40, read_uint40)
    };
    ($reader:expr, try_read_int64, read_int64) => {
        read_sync_first!(@pair $reader, try_read_int64, read_int64)
    };
    ($reader:expr, try_read_float32, read_float32) => {
        read_sync_first!(@pair $reader, try_read_float32, read_float32)
    };
    ($reader:expr, try_read_float64, read_float64) => {
        read_sync_first!(@pair $reader, try_read_float64, read_float64)
    };
    (@pair $reader:expr, $try_method:ident, $read_method:ident) => {{
        let reader = &mut *($reader);
        match reader.$try_method() {
            Some(value) => value,
            None => reader.$read_method().await?,
        }
    }};
}

// Offers the writer a destination for a PLP value's payload, falling back to an
// owned `Vec` when it declines.
//
// Only used for known-length PLP values. Short USHORTLEN values remain on the
// existing path so writers that decline destinations avoid an extra branch.
//
// The `None` arm expands to exactly the code that preceded this hook — allocate,
// fill, hand over — so a writer that does not opt in decodes through an
// unchanged instruction sequence. That is worth duplicating `$fill` across both
// arms: sharing one binding would keep the fallback `Vec` and a discriminant
// live across the read for every writer, opted in or not.
//
// Two reads means two suspension points in the caller's state machine, widening
// every row-decode future that embeds it by 192 B (`next_row_into` 3400 -> 3592,
// `read_row_column` 3752 -> 3944). Both remain within the 4096 B budget enforced
// by `row_fetch_futures_stay_small`; future size is disclosed, not used as a
// performance proxy.
//
// `$fill` reads the payload into `$dest` and yields `TdsResult<()>`; `$owned`
// hands the fallback buffer `$bytes` to the writer.
//
// The fallback allocates and fills inline rather than adding another async
// frame to the owned path.
macro_rules! read_value_into {
    (
        $writer:expr, $col:expr, $kind:expr, $length:expr,
        |$dest:ident| $fill:expr,
        |$bytes:ident| $owned:expr $(,)?
    ) => {{
        let col = $col;
        let length = $length;
        match $writer.value_destination(col, $kind, length) {
            // The writer borrow held by `$dest` ends with this read.
            Some($dest) => {
                debug_assert_eq!(
                    $dest.len(),
                    length,
                    "value_destination returned {} bytes for a {length}-byte value",
                    $dest.len()
                );
                let outcome = $fill;
                $writer.commit_value(col, outcome.is_ok());
                outcome?;
            }
            None => {
                let mut $bytes: Vec<u8> = Vec::with_capacity(length);
                {
                    let $dest: &mut [MaybeUninit<u8>] = &mut $bytes.spare_capacity_mut()[..length];
                    $fill?;
                }
                // SAFETY: `$fill` initialized all `length` bytes on the success path.
                unsafe { $bytes.set_len(length) };
                $owned;
            }
        }
    }};
}

/// Reads an encrypted column's cipher bytes from the wire and turns them back
/// into a plaintext [`ColumnValues`].
///
/// An Always Encrypted column is transmitted as `varbinary`, so the cipher blob
/// is decoded through the normal value path before being decrypted. A NULL cell
/// decrypts to [`ColumnValues::Null`].
///
/// The caller decides whether a column should be decrypted, so this is only
/// invoked with a live `decryptor`. Callers without one (Always Encrypted
/// disabled for the command, or no key store providers registered) decode the
/// raw ciphertext varbinary instead and never reach here.
pub(crate) async fn decrypt_encrypted_column<D, T>(
    decoder: &D,
    reader: &mut T,
    metadata: &ColumnMetadata,
    decryptor: &Arc<dyn CellDecryptor>,
) -> TdsResult<ColumnValues>
where
    D: SqlTypeDecode,
    T: TdsPacketReader + Send + Sync,
{
    let cipher = match decoder.decode(reader, metadata).await? {
        ColumnValues::Null => return Ok(ColumnValues::Null),
        ColumnValues::Bytes(bytes) => bytes,
        other => {
            return Err(crate::error::Error::ColumnEncryptionError(format!(
                "Encrypted column '{}' was expected to arrive as varbinary cipher bytes, but \
                 decoded as {other:?}",
                metadata.column_name
            )));
        }
    };

    let crypto_metadata = metadata.crypto_metadata.as_ref().ok_or_else(|| {
        crate::error::Error::ColumnEncryptionError(format!(
            "decrypt_encrypted_column called for non-encrypted column '{}'",
            metadata.column_name
        ))
    })?;

    decryptor.decrypt(crypto_metadata, &cipher)
}

// Maximum reasonable allocation size for a single value (100MB)
// This prevents fuzzer-induced capacity overflow panics
#[cfg(fuzzing)]
const MAX_ALLOC_SIZE: usize = 64 * 1024; // 64KB for fuzzing
#[cfg(not(fuzzing))]
const MAX_ALLOC_SIZE: usize = 100 * 1024 * 1024;

// Maximum allocation size for PLP (Partial Length Pointer) types
// SQL Server supports PLP types up to 2GB (i32::MAX is approximately 2.1GB)
#[cfg(fuzzing)]
const MAX_PLP_SIZE: usize = 64 * 1024; // 64KB for fuzzing
#[cfg(not(fuzzing))]
const MAX_PLP_SIZE: usize = i32::MAX as usize;

/// Maximum number of little-endian 32-bit words in a `decimal`/`numeric`
/// magnitude. SQL Server's maximum precision of 38 digits fits in 128 bits, and
/// the widest value the TDS wire format carries is 17 bytes (sign byte plus four
/// words), so anything longer is malformed.
const MAX_DECIMAL_INT_PARTS: usize = 4;

/// Byte width of a `decimal`/`numeric` magnitude: four little-endian 32-bit words.
pub(crate) const DECIMAL_MAGNITUDE_BYTES: usize = MAX_DECIMAL_INT_PARTS * 4;

// Helper function to validate allocation size before allocating
#[inline]
fn validate_alloc_size(size: usize, context: &str) -> TdsResult<()> {
    if size > MAX_ALLOC_SIZE {
        #[cfg(fuzzing)]
        {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "[ALLOC-REJECT] {} requesting {} bytes (max {})",
                context,
                size,
                MAX_ALLOC_SIZE
            );
        }

        return Err(crate::error::Error::ProtocolError(format!(
            "{context}: allocation size {size} exceeds maximum allowed {MAX_ALLOC_SIZE} bytes"
        )));
    }
    #[cfg(fuzzing)]
    {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "[ALLOC-OK] {} requesting {} bytes",
            context,
            size
        );
    }
    Ok(())
}

// Macro to create validated Vec allocations
#[cfg(fuzzing)]
macro_rules! safe_vec {
    ($elem:expr; $size:expr, $context:expr) => {{
        let size = $size;
        validate_alloc_size(size, $context)?;
        vec![$elem; size]
    }};
}

#[cfg(not(fuzzing))]
macro_rules! safe_vec {
    ($elem:expr; $size:expr, $context:expr) => {{
        let size = $size;
        validate_alloc_size(size, $context)?;
        vec![$elem; size]
    }};
}

pub(crate) trait SqlTypeDecode {
    fn decode<T>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
    ) -> impl Future<Output = TdsResult<ColumnValues>> + Send
    where
        T: TdsPacketReader + Send + Sync;
}

impl From<u8> for ColumnValues {
    fn from(value: u8) -> Self {
        ColumnValues::TinyInt(value)
    }
}

impl From<i32> for ColumnValues {
    fn from(value: i32) -> Self {
        ColumnValues::Int(value)
    }
}

#[derive(Debug, Default)]
pub(crate) struct GenericDecoder {
    string_decoder: StringDecoder,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlpChunkReadLength {
    Unknown,
    Known(u64),
}

/// Stateful PLP chunk reader for resumable/incremental consumption.
///
/// This keeps chunk cursor state so callers can read PLP payload in small
/// slices across repeated calls, msodbcsql-style streaming implementation.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct PlpChunkStreamReader {
    length: PlpChunkReadLength,
    chunk_remaining: usize,
    reached_end: bool,
    total_read: usize,
    chunks_seen: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PlpChunkStreamReader {
    fn new(length: PlpChunkReadLength) -> Self {
        Self {
            length,
            chunk_remaining: 0,
            reached_end: false,
            total_read: 0,
            chunks_seen: 0,
        }
    }

    pub(crate) async fn begin<T>(reader: &mut T) -> TdsResult<Option<Self>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let raw_len_i64 = read_sync_first!(reader, try_read_int64, read_int64);
        let raw_len = raw_len_i64 as u64;
        let raw_len_usize = raw_len as usize;

        if raw_len_usize == GenericDecoder::SQL_PLP_NULL {
            return Ok(None);
        }

        let length = if raw_len_usize == GenericDecoder::SQL_PLP_UNKNOWNLEN
            || raw_len_usize == GenericDecoder::SQL_PLP_MAXLEN
        {
            PlpChunkReadLength::Unknown
        } else {
            let declared_len = raw_len as usize;
            if raw_len_i64 < 0 || declared_len > MAX_PLP_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "PLP length {declared_len} (raw i64: {raw_len_i64}) exceeds maximum allowed size of {MAX_PLP_SIZE} bytes"
                )));
            }
            PlpChunkReadLength::Known(raw_len)
        };

        Ok(Some(Self::new(length)))
    }

    pub(crate) fn total_read(&self) -> usize {
        self.total_read
    }

    /// Declared total length of the whole PLP value in wire bytes when the
    /// server sent a known-length PLP header; `None` for unknown-length
    /// (streamed) PLP where the total is not known up front.
    pub(crate) fn known_len(&self) -> Option<u64> {
        match self.length {
            PlpChunkReadLength::Known(n) => Some(n),
            PlpChunkReadLength::Unknown => None,
        }
    }

    pub(crate) fn reached_end(&self) -> bool {
        self.reached_end
    }

    async fn ensure_active_chunk<T>(&mut self, reader: &mut T) -> TdsResult<bool>
    where
        T: TdsPacketReader + Send + Sync,
    {
        if self.reached_end {
            return Ok(false);
        }

        if self.chunk_remaining > 0 {
            return Ok(true);
        }

        let chunk_len = read_sync_first!(reader, try_read_uint32, read_uint32) as usize;
        if chunk_len == 0 {
            self.reached_end = true;
            if let PlpChunkReadLength::Known(known_len) = self.length
                && self.total_read != known_len as usize
            {
                return Err(crate::error::Error::ProtocolError(format!(
                    "PLP stream ended before declared length was reached: total_read={}, declared_len={known_len}",
                    self.total_read
                )));
            }
            return Ok(false);
        }

        self.chunks_seen += 1;
        if self.chunks_seen > GenericDecoder::MAX_PLP_CHUNKS {
            return Err(crate::error::Error::ProtocolError(format!(
                "Too many PLP chunks: {} (max {})",
                self.chunks_seen,
                GenericDecoder::MAX_PLP_CHUNKS
            )));
        }

        if chunk_len > GenericDecoder::MAX_PLP_CHUNK_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "PLP chunk size {chunk_len} exceeds maximum allowed chunk size of {} bytes",
                GenericDecoder::MAX_PLP_CHUNK_SIZE
            )));
        }

        let next_total = self.total_read.checked_add(chunk_len).ok_or_else(|| {
            crate::error::Error::ProtocolError(format!(
                "PLP chunk accumulation would overflow capacity: {} + {chunk_len}",
                self.total_read
            ))
        })?;

        if next_total > MAX_PLP_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "PLP accumulated size {next_total} exceeds maximum allowed size of {MAX_PLP_SIZE} bytes (SQL Server limit: 2GB)"
            )));
        }

        if let PlpChunkReadLength::Known(known_len) = self.length
            && next_total > known_len as usize
        {
            return Err(crate::error::Error::ProtocolError(format!(
                "PLP chunk exceeds declared length: accumulated={next_total}, declared_len={known_len}"
            )));
        }

        self.chunk_remaining = chunk_len;
        Ok(true)
    }

    pub(crate) async fn read_into<T>(&mut self, reader: &mut T, out: &mut [u8]) -> TdsResult<usize>
    where
        T: TdsPacketReader + Send + Sync,
    {
        // Supports the msodbcsql-style cbRequest==0 pattern to consume a
        // pending terminator after all data bytes were already read.
        if out.is_empty() {
            let _ = self.ensure_active_chunk(reader).await?;
            return Ok(0);
        }

        let mut written = 0;
        while written < out.len() {
            if !self.ensure_active_chunk(reader).await? {
                break;
            }

            let to_read = std::cmp::min(out.len() - written, self.chunk_remaining);
            let slice = &mut out[written..written + to_read];
            let bytes_read = reader.read_bytes(slice).await?;

            if to_read > 0 && bytes_read == 0 {
                return Err(crate::error::Error::ProtocolError(
                    "PLP stream read made no progress while bytes were requested".to_string(),
                ));
            }

            self.chunk_remaining -= bytes_read;
            self.total_read += bytes_read;
            written += bytes_read;
        }

        // If we ended exactly on a chunk boundary after filling caller buffer,
        // consume a pending zero-length terminator now to align with msodbcsql.
        if written == out.len() && self.chunk_remaining == 0 && !self.reached_end {
            let _ = self.ensure_active_chunk(reader).await?;
        }

        Ok(written)
    }

    pub(crate) async fn skip_to_end<T>(&mut self, reader: &mut T) -> TdsResult<()>
    where
        T: TdsPacketReader + Send + Sync,
    {
        while self.ensure_active_chunk(reader).await? {
            if self.chunk_remaining > 0 {
                reader.skip_bytes(self.chunk_remaining).await?;
                self.total_read += self.chunk_remaining;
                self.chunk_remaining = 0;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Type-aware PLP column stream
// ---------------------------------------------------------------------------

/// A type-aware, metadata-bound PLP column reader.
///
/// Wraps [`PlpChunkStreamReader`] and carries the semantic type information
/// to correctly interpret and convert the raw chunk bytes returned by the
/// TDS server. The wire-level chunk framing is fully handled by the inner
/// reader; this struct only adds type context.
///
/// The PLP type is represented directly as [`PartialLengthType`], which already
/// covers all six PLP-capable SQL Server types (`varchar(max)`, `nvarchar(max)`,
/// `varbinary(max)`, `xml`, `json`, `udt`). An optional [`SqlCollation`] is
/// carried alongside from PLP metadata (typically for `varchar(max)`), for
/// caller-side where collation-aware decoding may be needed.
///
/// # Usage
///
/// ```ignore
/// let stream = PlpColumnStream::begin(&col_metadata, &mut tds_reader).await?;
/// match stream {
///     None => { /* SQL NULL */ }
///     Some(mut s) => {
///         let mut buf = [0u8; 4096];
///         while !s.reached_end() {
///             let n = s.read_into(&mut tds_reader, &mut buf).await?;
///             // use s.plp_type() and s.collation() for type/encoding decisions
///         }
///     }
/// }
/// ```
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct PlpColumnStream {
    plp_type: PartialLengthType,
    collation: Option<SqlCollation>,
    inner: PlpChunkStreamReader,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PlpColumnStream {
    /// Reads the 8-byte PLP length/sentinel header and returns:
    /// - `Ok(None)` for SQL NULL
    /// - `Ok(Some(stream))` ready for incremental reads
    /// - `Err` if the column is not PLP-typed or the header is malformed
    pub(crate) async fn begin<T>(
        metadata: &ColumnMetadata,
        reader: &mut T,
    ) -> TdsResult<Option<Self>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let (plp_type, collation) = Self::type_from_metadata(metadata)?;
        let inner = match PlpChunkStreamReader::begin(reader).await? {
            None => return Ok(None),
            Some(r) => r,
        };
        Ok(Some(Self {
            plp_type,
            collation,
            inner,
        }))
    }

    /// The PLP-capable SQL Server type for this column.
    pub(crate) fn plp_type(&self) -> PartialLengthType {
        self.plp_type
    }

    /// Collation from PLP metadata when provided (typically `varchar(max)` / `BigVarChar`).
    pub(crate) fn collation(&self) -> Option<SqlCollation> {
        self.collation
    }

    /// Total payload bytes consumed so far across all chunks.
    pub(crate) fn total_read(&self) -> usize {
        self.inner.total_read()
    }

    /// Declared total length of the whole PLP value in wire bytes, when the
    /// server sent a known-length PLP header; `None` for unknown-length PLP.
    pub(crate) fn known_len(&self) -> Option<u64> {
        self.inner.known_len()
    }

    /// `true` after the zero-length terminator chunk has been consumed.
    pub(crate) fn reached_end(&self) -> bool {
        self.inner.reached_end()
    }

    /// Incrementally reads PLP payload bytes into `out`.
    pub(crate) async fn read_into<T>(&mut self, reader: &mut T, out: &mut [u8]) -> TdsResult<usize>
    where
        T: TdsPacketReader + Send + Sync,
    {
        self.inner.read_into(reader, out).await
    }

    /// Discards all remaining PLP payload and terminator bytes.
    pub(crate) async fn skip_to_end<T>(&mut self, reader: &mut T) -> TdsResult<()>
    where
        T: TdsPacketReader + Send + Sync,
    {
        self.inner.skip_to_end(reader).await
    }

    /// Extracts the `PartialLengthType` and optional collation from column metadata.
    ///
    /// Returns `Err` if the column is not a PLP (partial-length-prefixed) type.
    fn type_from_metadata(
        metadata: &ColumnMetadata,
    ) -> TdsResult<(PartialLengthType, Option<SqlCollation>)> {
        match metadata.type_info.type_info_variant {
            TypeInfoVariant::PartialLen(pt, _, collation, _, _) => Ok((pt, collation)),
            _ => Err(crate::error::Error::ProtocolError(format!(
                "Column '{}' (type {:?}) is not a PLP type",
                metadata.column_name, metadata.data_type
            ))),
        }
    }
}

impl GenericDecoder {
    #[cfg(test)]
    const SHORTLEN_MAXVALUE: usize = 65535;
    const SQL_PLP_NULL: usize = 0xffffffffffffffff;
    const SQL_PLP_UNKNOWNLEN: usize = 0xfffffffffffffffe;
    #[cfg_attr(not(test), allow(dead_code))]
    const SQL_PLP_MAXLEN: usize = 0xfffffffffffffffd;
    #[cfg(fuzzing)]
    const MAX_PLP_CHUNKS: u32 = 1000;
    #[cfg(not(fuzzing))]
    const MAX_PLP_CHUNKS: u32 = 100000;
    #[cfg(fuzzing)]
    const MAX_PLP_CHUNK_SIZE: usize = 8 * 1024;
    #[cfg(not(fuzzing))]
    const MAX_PLP_CHUNK_SIZE: usize = 16 * 1024 * 1024;

    /// Boxed re-entry into [`SqlTypeDecode::decode`], used only by SQL_VARIANT.
    ///
    /// SQL_VARIANT embeds a base type, so decoding one re-enters the type switch. Native
    /// `async fn` cannot express that cycle: the opaque return type would contain itself.
    /// Naming a concrete `Pin<Box<dyn Future>>` in the signature severs the dependency, at
    /// the cost of one allocation per nested variant column — a rare type, never on the hot
    /// path of ordinary scalar columns.
    fn decode_boxed<'a, T>(
        &'a self,
        reader: &'a mut T,
        metadata: &'a ColumnMetadata,
    ) -> Pin<Box<dyn Future<Output = TdsResult<ColumnValues>> + Send + 'a>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        Box::pin(self.decode(reader, metadata))
    }

    // Reads a SQL_VARIANT type from the TDS stream.
    async fn read_sql_variant<T>(&self, reader: &mut T) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        self.read_sql_variant_with_base(reader)
            .await
            .map(|(_, value)| value)
    }

    /// As [`Self::read_sql_variant`], but also returns the base type the variant
    /// declared on the wire. The decoded value alone cannot always recover it —
    /// `varchar` and `nvarchar` both arrive as [`ColumnValues::String`]. The base
    /// is `None` for a NULL variant, which carries no type on the wire.
    async fn read_sql_variant_with_base<T>(
        &self,
        reader: &mut T,
    ) -> TdsResult<(Option<TdsDataType>, ColumnValues)>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let length = read_sync_first!(reader, try_read_uint32, read_uint32);
        // A NULL variant is the zero length and nothing else: reading a base type
        // and property byte here would consume the next token's bytes.
        if length == 0 {
            return Ok((None, ColumnValues::Null));
        }
        let variant_base_type = read_sync_first!(reader, try_read_byte, read_byte);
        let tds_type = TdsDataType::try_from(variant_base_type)?;
        let variant_prop_bytes = read_sync_first!(reader, try_read_byte, read_byte);
        let bytes_for_type_and_properties_byte = 2;

        // Use checked arithmetic to prevent integer underflow
        let data_length = length
            .checked_sub(bytes_for_type_and_properties_byte)
            .and_then(|v| v.checked_sub(variant_prop_bytes as u32))
            .ok_or_else(|| {
                crate::error::Error::ProtocolError(format!(
                    "SQL_VARIANT data length calculation underflow: length={length}, prop_bytes={variant_prop_bytes}"
                ))
            })?;

        let col_value = match variant_prop_bytes {
            0 => {
                self.decode_zero_propbyte_variant(reader, tds_type, data_length)
                    .await?
            }
            1 => {
                // TIMENTYPE, DATETIME2NTYPE, DATETIMEOFFSETNTYPE
                self.decode_one_byte_variant(reader, tds_type, data_length)
                    .await?
            }
            2 => {
                decode_two_propbyte_variant(reader, variant_base_type, tds_type, data_length)
                    .await?
            }
            7 => {
                // BIGVARCHARTYPE, BIGCHARTYPE, NVARCHARTYPE, NCHARTYPE
                decode_seven_propbyte_variant(reader, tds_type, data_length).await?
            }
            _ => {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Unexpected SQL variant properties length: {variant_prop_bytes}. Expected 0, 1, 2, or 7. This indicates malformed or invalid data."
                )));
            }
        };
        Ok((Some(tds_type), col_value))
    }

    async fn decode_zero_propbyte_variant<T>(
        &self,
        reader: &mut T,
        tds_type: TdsDataType,
        data_length: u32,
    ) -> Result<ColumnValues, crate::error::Error>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let fixed_length_type_result = FixedLengthTypes::try_from(tds_type);

        // The type may be a fixed length type, or it may be a variable length type like Guid/DateN
        match fixed_length_type_result {
            Ok(fixed_length_type) => {
                let type_info = TypeInfo {
                    tds_type,
                    length: data_length as usize,
                    type_info_variant: TypeInfoVariant::FixedLen(fixed_length_type),
                };
                let variant_actual_type_md = ColumnMetadata {
                    user_type: 0,
                    flags: 0,
                    type_info,
                    data_type: tds_type,
                    column_name: "".to_string(),
                    multi_part_name: None,
                    crypto_metadata: None,
                };
                self.decode_boxed(reader, &variant_actual_type_md).await
            }
            _ => {
                // If the type is not a fixed length type, we should not reach here.
                match tds_type {
                    TdsDataType::Guid => Self::read_guid(reader, data_length as u8).await,
                    TdsDataType::DateN => Self::read_daten(reader, data_length as u8).await,
                    _ => Err(crate::error::Error::ProtocolError(format!(
                        "For 0 byte property, only Guid and DateN are expected, but got: {tds_type:?}"
                    ))),
                }
            }
        }
    }

    async fn decode_one_byte_variant<T>(
        &self,
        reader: &mut T,
        tds_type: TdsDataType,
        data_length: u32,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let scale = read_sync_first!(reader, try_read_byte, read_byte);
        Ok(match tds_type {
            TdsDataType::TimeN => {
                let time_nanos = self.read_time(reader, data_length as u8, scale).await?;
                ColumnValues::Time(time_nanos)
            }
            TdsDataType::DateTime2N => {
                self.read_datetime2(reader, data_length as u8, scale)
                    .await?
            }
            TdsDataType::DateTimeOffsetN => {
                self.read_datetime_offset(reader, data_length as u8, scale)
                    .await?
            }
            _ => {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Invalid SQL_VARIANT: 1-byte property is only valid for TimeN, DateTime2N, and DateTimeOffsetN types, but got: {tds_type:?}"
                )));
            }
        })
    }

    async fn read_decimal<T>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
    ) -> TdsResult<Option<DecimalParts>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        // Decimal/numeric data type has 1 byte length.
        let length = read_sync_first!(reader, try_read_byte, read_byte);
        let TypeInfoVariant::VarLenPrecisionScale(_, _, precision, scale) =
            metadata.type_info.type_info_variant
        else {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid type info variant for Decimal/Numeric: expected VarLenPrecisionScale, got: {:?}",
                metadata.type_info.type_info_variant
            )));
        };
        GenericDecoder::read_decimal_data(reader, length, precision, scale).await
    }

    async fn read_decimal_data<T>(
        reader: &mut T,
        length: u8,
        precision: u8,
        scale: u8,
    ) -> TdsResult<Option<DecimalParts>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        // If length is 0, then it is NULL.
        if length == 0 {
            return Ok(None);
        }
        let sign = read_sync_first!(reader, try_read_byte, read_byte);
        let is_positive = sign == 1;

        // Round up: a declared length that does not cover whole 32-bit words
        // still has to be consumed, or the leftover bytes desynchronize the
        // stream for the next field. A trailing partial word is zero-padded,
        // which matches the length-tolerant reader on the Always Encrypted path.
        let magnitude_len = (length - 1) as usize;
        let number_of_int_parts = magnitude_len.div_ceil(4);

        if number_of_int_parts > MAX_DECIMAL_INT_PARTS {
            return Err(crate::error::Error::ProtocolError(format!(
                "Decimal int parts {number_of_int_parts} exceeds maximum allowed {MAX_DECIMAL_INT_PARTS} (length was {length})"
            )));
        }

        // The check above bounds the payload at 16 bytes, so it is read onto the
        // stack. TDS is little-endian and the array is zero-filled, which both
        // reassembles the magnitude and zero-pads a partial trailing word.
        let mut magnitude = [0u8; DECIMAL_MAGNITUDE_BYTES];
        reader.read_bytes(&mut magnitude[..magnitude_len]).await?;

        Ok(Some(DecimalParts::new(
            is_positive,
            precision,
            scale,
            u128::from_le_bytes(magnitude),
        )))
    }

    async fn read_datetime<T>(&self, reader: &mut T) -> TdsResult<SqlDateTime>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let days = read_sync_first!(reader, try_read_int32, read_int32);
        let ticks = read_sync_first!(reader, try_read_uint32, read_uint32);

        Ok(SqlDateTime { days, time: ticks })
    }

    async fn read_small_datetime<T>(&self, reader: &mut T) -> TdsResult<SqlSmallDateTime>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let days = read_sync_first!(reader, try_read_uint16, read_uint16);
        let minutes = read_sync_first!(reader, try_read_uint16, read_uint16);
        Ok(SqlSmallDateTime {
            days,
            time: minutes,
        })
    }

    async fn read_date<T>(reader: &mut T) -> TdsResult<SqlDate>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let days = read_sync_first!(reader, try_read_uint24, read_uint24);
        Ok(SqlDate::unchecked_create(days))
    }

    async fn read_time<T>(&self, reader: &mut T, byte_len: u8, scale: u8) -> TdsResult<SqlTime>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let scaled_value = match byte_len {
            3 => read_sync_first!(reader, try_read_uint24, read_uint24) as u64,
            4 => read_sync_first!(reader, try_read_uint32, read_uint32) as u64,
            _ => read_sync_first!(reader, try_read_uint40, read_uint40),
        };

        // The value from SQL Server is in scaled units based on the scale:
        // Scale 0: seconds (need to multiply by 10^7)
        // Scale 1: tenths of seconds (multiply by 10^6)
        // Scale 2: hundredths (multiply by 10^5)
        // Scale 3: milliseconds (multiply by 10^4)
        // Scale 4: ten-thousandths (multiply by 10^3)
        // Scale 5: hundred-thousandths (multiply by 10^2)
        // Scale 6: microseconds (multiply by 10^1)
        // Scale 7: 100-nanoseconds (multiply by 10^0 = no scaling)
        // We need to convert to 100-nanosecond units for consistency
        let time_nanoseconds = match scale {
            0 => scaled_value * 10_000_000, // Seconds to 100ns
            1 => scaled_value * 1_000_000,  // Tenths to 100ns
            2 => scaled_value * 100_000,    // Hundredths to 100ns
            3 => scaled_value * 10_000,     // Milliseconds to 100ns
            4 => scaled_value * 1_000,      // Ten-thousandths to 100ns
            5 => scaled_value * 100,        // Hundred-thousandths to 100ns
            6 => scaled_value * 10,         // Microseconds to 100ns
            7 => scaled_value,              // Already in 100ns
            _ => scaled_value,              // Fallback
        };

        Ok(SqlTime {
            time_nanoseconds,
            scale,
        })
    }

    async fn read_datetime2<T>(
        &self,
        reader: &mut T,
        byte_len: u8,
        scale: u8,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let time_byte_len = byte_len.checked_sub(3).ok_or_else(|| {
            crate::error::Error::ProtocolError(format!(
                "Invalid DateTime2 byte length: {byte_len}. Expected at least 3 bytes for date component."
            ))
        })?;
        let time_nanos = self.read_time(reader, time_byte_len, scale).await?;
        let sql_date = Self::read_date(reader).await?;
        let datetime2 = SqlDateTime2 {
            days: sql_date.get_days(),
            time: time_nanos,
        };
        Ok(ColumnValues::DateTime2(datetime2))
    }

    async fn read_datetime_offset<T>(
        &self,
        reader: &mut T,
        byte_len: u8,
        scale: u8,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let datetime2_byte_len = byte_len.checked_sub(2).ok_or_else(|| {
            crate::error::Error::ProtocolError(format!(
                "Invalid DateTimeOffset byte length: {byte_len}. Expected at least 2 bytes for offset component."
            ))
        })?;
        let datetime2 = self
            .read_datetime2(reader, datetime2_byte_len, scale)
            .await?;
        let datetime2 = match datetime2 {
            ColumnValues::DateTime2(dt2) => dt2,
            _ => {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Internal error: read_datetime2 returned unexpected type: {datetime2:?}"
                )));
            }
        };
        let offset = read_sync_first!(reader, try_read_int16, read_int16);
        let datetime_offset = SqlDateTimeOffset { datetime2, offset };
        Ok(ColumnValues::DateTimeOffset(datetime_offset))
    }

    async fn read_intn<T>(&self, reader: &mut T, byte_len: u8) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let value: ColumnValues = match byte_len {
            1 => ColumnValues::TinyInt(read_sync_first!(reader, try_read_byte, read_byte)),
            2 => ColumnValues::SmallInt(read_sync_first!(reader, try_read_int16, read_int16)),
            4 => ColumnValues::Int(read_sync_first!(reader, try_read_int32, read_int32)),
            8 => ColumnValues::BigInt(read_sync_first!(reader, try_read_int64, read_int64)),
            0 => ColumnValues::Null,
            _ => {
                return Err(crate::error::Error::from(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid IntN length",
                )));
            }
        };
        Ok(value)
    }

    async fn read_money4<T>(&self, reader: &mut T) -> TdsResult<SqlSmallMoney>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let small_money_val = read_sync_first!(reader, try_read_int32, read_int32);
        Ok(small_money_val.into())
    }

    // Reads the TDS 8-byte money value. It is represented in TDS as two 4-byte integers (mixed endian).
    // See comments in MoneyParts definition for more details.
    async fn read_money8<T>(&self, reader: &mut T) -> TdsResult<SqlMoney>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let msb = read_sync_first!(reader, try_read_int32, read_int32);
        let lsb = read_sync_first!(reader, try_read_int32, read_int32);
        Ok(SqlMoney {
            lsb_part: lsb,
            msb_part: msb,
        })
    }

    async fn read_daten<T>(reader: &mut T, length: u8) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        if length == 0 {
            Ok(ColumnValues::Null)
        } else {
            // length == 3.
            Ok(ColumnValues::Date(Self::read_date(reader).await?))
        }
    }

    async fn read_guid<T>(reader: &mut T, length: u8) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        if length > 0 {
            // UUID must be exactly 16 bytes
            if length != 16 {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Invalid GUID length: expected 16 bytes, got {length}"
                )));
            }
            let mut bytes = safe_vec![0u8; length as usize, "read_guid"];
            reader.read_bytes(&mut bytes).await?;
            let unique_id = uuid::Uuid::from_slice_le(&bytes).map_err(|e| {
                crate::error::Error::ProtocolError(format!("Failed to parse UUID: {e}"))
            })?;
            Ok(ColumnValues::Uuid(unique_id))
        } else {
            Ok(ColumnValues::Null)
        }
    }

    async fn decode_vector<T>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        use crate::datatypes::sql_vector::SqlVector;
        use crate::datatypes::sqldatatypes::{
            VECTOR_HEADER_SIZE, VECTOR_MAX_SIZE, VectorBaseType, VectorLayoutFormat,
            VectorLayoutVersion,
        };

        // Read length prefix (USHORTLEN format)
        let length_prefix_value = read_sync_first!(reader, try_read_uint16, read_uint16) as usize;

        // Handle NULL (length = 0xFFFF)
        if length_prefix_value == 0xFFFF {
            return Ok(ColumnValues::Null);
        }

        // Validate length
        if length_prefix_value > VECTOR_MAX_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector length {} exceeds maximum of {} bytes",
                length_prefix_value, VECTOR_MAX_SIZE
            )));
        }

        // Must have at least header
        if length_prefix_value < VECTOR_HEADER_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector length {} is less than minimum header size of {} bytes",
                length_prefix_value, VECTOR_HEADER_SIZE
            )));
        }

        // Read 8-byte header
        let layout_format_byte = read_sync_first!(reader, try_read_byte, read_byte);
        let layout_version_byte = read_sync_first!(reader, try_read_byte, read_byte);
        let dimension_count = read_sync_first!(reader, try_read_uint16, read_uint16);
        let base_type_byte = read_sync_first!(reader, try_read_byte, read_byte);
        let _reserved1 = read_sync_first!(reader, try_read_byte, read_byte); // Reserved
        let _reserved2 = read_sync_first!(reader, try_read_byte, read_byte); // Reserved
        let _reserved3 = read_sync_first!(reader, try_read_byte, read_byte); // Reserved

        // Validate header using enum conversions
        let _layout_format = VectorLayoutFormat::try_from(layout_format_byte)?;
        let _layout_version = VectorLayoutVersion::try_from(layout_version_byte)?;

        // Get base type from metadata's TypeInfoVariant scale field
        let base_type_in_metadata = match &metadata.type_info.type_info_variant {
            TypeInfoVariant::VarLenScale(_, scale) => *scale,
            _ => {
                return Err(crate::error::Error::ProtocolError(
                    "Vector metadata missing scale (base type)".to_string(),
                ));
            }
        };

        if base_type_byte != base_type_in_metadata {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector base type mismatch: metadata has 0x{:02X}, vector header has 0x{:02X}",
                base_type_in_metadata, base_type_byte
            )));
        }

        // Validate base type using enum conversion
        let base_type = VectorBaseType::try_from(base_type_byte)?;

        let length_in_metadata = metadata.type_info.length;
        // Calculate data length based on vector header info
        let element_size = base_type.element_size_bytes();
        let length_from_vector_header =
            VECTOR_HEADER_SIZE + (dimension_count as usize * element_size);
        if length_prefix_value != length_from_vector_header
            || length_prefix_value != length_in_metadata
        {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector length mismatch: length in prefix {} bytes, length from vector header {} bytes, length in metadata {} bytes, for {} dimensions (element size: {} bytes)",
                length_prefix_value,
                length_from_vector_header,
                length_in_metadata,
                dimension_count,
                element_size
            )));
        }

        // Read raw element bytes (let SqlVector parse based on base_type)
        let element_bytes = length_prefix_value - VECTOR_HEADER_SIZE;
        let mut raw_bytes = vec![0u8; element_bytes];
        reader.read_bytes(&mut raw_bytes).await?;

        // Create SqlVector - try_from_raw validates header, parses bytes by type, and validates dimensions
        let vector = SqlVector::try_from_raw(
            layout_format_byte,
            layout_version_byte,
            base_type_byte,
            raw_bytes,
        )?;

        Ok(ColumnValues::Vector(vector))
    }

    /// Reads a whole PLP value into an owned buffer, or `None` for `PLP_NULL`.
    ///
    /// Kept for the value paths that must produce a `ColumnValues` anyway (XML,
    /// JSON, vector, image, UDT, and all of `decode`). It shares its framing and
    /// chunk handling with the writer-facing path so the two cannot disagree
    /// about what the wire said.
    async fn read_plp_bytes<T>(reader: &mut T) -> TdsResult<Option<Vec<u8>>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        match Self::read_plp_framing(reader).await? {
            PlpFraming::Null => Ok(None),
            PlpFraming::Known(length) => {
                let mut plp_buffer = vec![0u8; length];
                // SAFETY: initialized bytes are valid `MaybeUninit<u8>` values.
                let dest = unsafe {
                    std::slice::from_raw_parts_mut(
                        plp_buffer.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                        plp_buffer.len(),
                    )
                };
                Self::read_plp_chunks_into_slice(reader, dest, length).await?;
                Ok(Some(plp_buffer))
            }
            PlpFraming::Unknown => Ok(Some(Self::read_plp_chunks_unknown_len(reader).await?)),
        }
    }

    /// Reads a `SQL_PLP_UNKNOWNLEN` chunk stream, growing the buffer as chunks
    /// arrive until the terminating zero-length chunk.
    async fn read_plp_chunks_unknown_len<T>(reader: &mut T) -> TdsResult<Vec<u8>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let mut plp_buffer = Vec::new();
        let mut vector_capacity: usize = 0;
        let mut chunk_len = read_sync_first!(reader, try_read_uint32, read_uint32) as usize;
        let mut offset: usize = 0;
        let mut chunk_count = 0u32;

        while chunk_len > 0 {
            chunk_count += 1;

            #[cfg(fuzzing)]
            {
                eprintln!(
                    "[ALLOC] read_plp_chunks_unknown_len: chunk #{chunk_count}, chunk_len={chunk_len}, total_capacity={vector_capacity}"
                );
            }

            Self::check_plp_chunk(chunk_count, chunk_len)?;

            // Use checked_add to prevent capacity overflow
            vector_capacity = vector_capacity.checked_add(chunk_len).ok_or_else(|| {
                crate::error::Error::ProtocolError(format!(
                    "PLP chunk accumulation would overflow capacity: {vector_capacity} + {chunk_len}"
                ))
            })?;
            // Validate against MAX_PLP_SIZE after accumulation
            if vector_capacity > MAX_PLP_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "PLP accumulated size {vector_capacity} exceeds maximum allowed size of {MAX_PLP_SIZE} bytes (SQL Server limit: 2GB)"
                )));
            }
            plp_buffer.resize(vector_capacity, 0);

            offset += reader
                .read_bytes(&mut plp_buffer[offset..offset + chunk_len])
                .await?;
            chunk_len = read_sync_first!(reader, try_read_uint32, read_uint32) as usize;
        }

        Ok(plp_buffer)
    }

    /// Validates a PLP chunk header against the per-value chunk limits.
    fn check_plp_chunk(chunk_count: u32, chunk_len: usize) -> TdsResult<()> {
        if chunk_count > Self::MAX_PLP_CHUNKS {
            return Err(crate::error::Error::ProtocolError(format!(
                "Too many PLP chunks: {chunk_count} (max {})",
                Self::MAX_PLP_CHUNKS
            )));
        }

        // Limit individual chunk size
        if chunk_len > Self::MAX_PLP_CHUNK_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "PLP chunk size {chunk_len} exceeds maximum allowed chunk size of {} bytes",
                Self::MAX_PLP_CHUNK_SIZE
            )));
        }

        Ok(())
    }

    /// Reads the length prefix of a PLP value without consuming its payload.
    async fn read_plp_framing<T>(reader: &mut T) -> TdsResult<PlpFraming>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let long_len_i64 = read_sync_first!(reader, try_read_int64, read_int64);
        let long_len = long_len_i64 as u64;

        if long_len as usize == Self::SQL_PLP_NULL {
            return Ok(PlpFraming::Null);
        }
        if long_len as usize == Self::SQL_PLP_UNKNOWNLEN {
            return Ok(PlpFraming::Unknown);
        }

        let capacity = long_len as usize;
        // A negative i64 cast to usize produces a huge value, so check the sign too.
        if long_len_i64 < 0 || capacity > MAX_PLP_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "PLP length {capacity} (raw i64: {long_len_i64}) exceeds maximum allowed size of {MAX_PLP_SIZE} bytes"
            )));
        }
        Ok(PlpFraming::Known(capacity))
    }

    /// Reads the chunk stream of a known-length PLP value straight into `dest`.
    ///
    /// A stream that ends before filling `dest` leaves the remainder zeroed, so
    /// a short value reaches the writer exactly as it would on the owned path
    /// rather than exposing whatever the destination held beforehand.
    async fn read_plp_chunks_into_slice<T>(
        reader: &mut T,
        dest: &mut [MaybeUninit<u8>],
        declared_len: usize,
    ) -> TdsResult<()>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let mut chunk_len = read_sync_first!(reader, try_read_uint32, read_uint32) as usize;
        let mut offset: usize = 0;
        let mut chunk_count = 0u32;

        while chunk_len > 0 {
            chunk_count += 1;
            #[cfg(fuzzing)]
            eprintln!(
                "[ALLOC] read_plp_chunks_into_slice: chunk #{chunk_count}, chunk_len={chunk_len}, wire_declared_len={declared_len}, destination_len={}",
                dest.len()
            );
            Self::check_plp_chunk(chunk_count, chunk_len)?;

            let end_offset = offset.checked_add(chunk_len).ok_or_else(|| {
                crate::error::Error::ProtocolError(format!(
                    "PLP chunk offset would overflow: {offset} + {chunk_len}"
                ))
            })?;
            if end_offset > declared_len || end_offset > dest.len() {
                return Err(crate::error::Error::ProtocolError(format!(
                    "PLP chunk exceeds wire-declared length: offset={offset}, chunk_len={chunk_len}, wire_declared_len={declared_len}, destination_len={}",
                    dest.len(),
                )));
            }

            offset += reader
                .read_bytes_uninit(&mut dest[offset..end_offset])
                .await?;
            chunk_len = read_sync_first!(reader, try_read_uint32, read_uint32) as usize;
        }

        dest[offset..].fill(MaybeUninit::new(0));
        Ok(())
    }

    /// Decodes a column value from the wire and writes it directly into a
    /// [`RowWriter`], bypassing the intermediate `ColumnValues` enum for
    /// common types. Rare types (XML, JSON, Vector, Image, UDT, SsVariant)
    /// fall back to `decode()` + `write_column_value()`.
    pub(crate) async fn decode_into<T, W>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
        col: usize,
        writer: &mut W,
    ) -> TdsResult<()>
    where
        T: TdsPacketReader + Send + Sync,
        W: RowWriter + ?Sized,
    {
        match metadata.data_type {
            // === Fixed-length integer types ===
            TdsDataType::Int1 => {
                writer.write_u8(col, read_sync_first!(reader, try_read_byte, read_byte));
            }
            TdsDataType::Int2 => {
                writer.write_i16(col, read_sync_first!(reader, try_read_int16, read_int16));
            }
            TdsDataType::Int4 => {
                writer.write_i32(col, read_sync_first!(reader, try_read_int32, read_int32));
            }
            TdsDataType::Int8 => {
                writer.write_i64(col, read_sync_first!(reader, try_read_int64, read_int64));
            }
            TdsDataType::IntN => {
                let byte_len = read_sync_first!(reader, try_read_byte, read_byte);
                match byte_len {
                    1 => writer.write_u8(col, read_sync_first!(reader, try_read_byte, read_byte)),
                    2 => {
                        writer.write_i16(col, read_sync_first!(reader, try_read_int16, read_int16))
                    }
                    4 => {
                        writer.write_i32(col, read_sync_first!(reader, try_read_int32, read_int32))
                    }
                    8 => {
                        writer.write_i64(col, read_sync_first!(reader, try_read_int64, read_int64))
                    }
                    0 => writer.write_null(col),
                    _ => {
                        return Err(crate::error::Error::from(Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Invalid IntN length",
                        )));
                    }
                }
            }

            // === Fixed-length float types ===
            TdsDataType::Flt4 => {
                writer.write_f32(
                    col,
                    read_sync_first!(reader, try_read_float32, read_float32),
                );
            }
            TdsDataType::Flt8 => {
                writer.write_f64(
                    col,
                    read_sync_first!(reader, try_read_float64, read_float64),
                );
            }
            TdsDataType::FltN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                match length {
                    0 => writer.write_null(col),
                    4 => writer.write_f32(
                        col,
                        read_sync_first!(reader, try_read_float32, read_float32),
                    ),
                    _ => writer.write_f64(
                        col,
                        read_sync_first!(reader, try_read_float64, read_float64),
                    ),
                }
            }

            // === Bit types ===
            TdsDataType::Bit => {
                writer.write_bool(col, read_sync_first!(reader, try_read_byte, read_byte) == 1);
            }
            TdsDataType::BitN => {
                let byte_len = read_sync_first!(reader, try_read_byte, read_byte);
                if byte_len > 0 {
                    writer.write_bool(col, read_sync_first!(reader, try_read_byte, read_byte) == 1);
                } else {
                    writer.write_null(col);
                }
            }

            // === Money types ===
            TdsDataType::Money4 => {
                writer.write_smallmoney(col, self.read_money4(reader).await?);
            }
            TdsDataType::Money => {
                writer.write_money(col, self.read_money8(reader).await?);
            }
            TdsDataType::MoneyN => {
                let byte_len = read_sync_first!(reader, try_read_byte, read_byte);
                match byte_len {
                    4 => writer.write_smallmoney(col, self.read_money4(reader).await?),
                    8 => writer.write_money(col, self.read_money8(reader).await?),
                    0 => writer.write_null(col),
                    _ => {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Invalid MoneyN length - {byte_len}"
                        )));
                    }
                }
            }

            // === Decimal / Numeric ===
            TdsDataType::DecimalN => match self.read_decimal(reader, metadata).await? {
                Some(val) => writer.write_decimal(col, val),
                None => writer.write_null(col),
            },
            TdsDataType::NumericN => match self.read_decimal(reader, metadata).await? {
                Some(val) => writer.write_numeric(col, val),
                None => writer.write_null(col),
            },

            // === String types — delegate to StringDecoder ===
            TdsDataType::NChar
            | TdsDataType::NVarChar
            | TdsDataType::BigChar
            | TdsDataType::BigVarChar
            | TdsDataType::Char
            | TdsDataType::VarChar
            | TdsDataType::NText
            | TdsDataType::Text => {
                self.string_decoder
                    .decode_string_into(reader, metadata, col, writer)
                    .await?;
            }

            // === Binary types ===
            TdsDataType::BigBinary => {
                let length = read_sync_first!(reader, try_read_uint16, read_uint16);
                // 0xFFFF is the USHORTLEN NULL marker (CHARBIN_NULL).
                if length == 0xFFFF {
                    writer.write_null(col);
                } else {
                    if length as usize > MAX_ALLOC_SIZE {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "BigBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                        )));
                    }
                    let mut bytes = vec![0u8; length as usize];
                    reader.read_bytes(&mut bytes).await?;
                    writer.write_bytes(col, bytes);
                }
            }
            TdsDataType::BigVarBinary => {
                if metadata.is_plp() {
                    match GenericDecoder::read_plp_framing(reader).await? {
                        PlpFraming::Null => writer.write_null(col),
                        PlpFraming::Known(length) => {
                            read_value_into!(
                                writer,
                                col,
                                ValueKind::Bytes,
                                length,
                                |dest| GenericDecoder::read_plp_chunks_into_slice(
                                    reader, dest, length,
                                )
                                .await,
                                |bytes| writer.write_bytes(col, bytes),
                            );
                        }
                        PlpFraming::Unknown => {
                            writer.write_bytes(
                                col,
                                GenericDecoder::read_plp_chunks_unknown_len(reader).await?,
                            );
                        }
                    }
                } else {
                    let length = read_sync_first!(reader, try_read_uint16, read_uint16);
                    // 0xFFFF is the USHORTLEN NULL marker (CHARBIN_NULL).
                    if length == 0xFFFF {
                        writer.write_null(col);
                    } else {
                        if length as usize > MAX_ALLOC_SIZE {
                            return Err(crate::error::Error::ProtocolError(format!(
                                "BigVarBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                            )));
                        }
                        let mut bytes = vec![0u8; length as usize];
                        reader.read_bytes(&mut bytes).await?;
                        writer.write_bytes(col, bytes);
                    }
                }
            }

            // === DateTime types ===
            TdsDataType::DateTime => {
                writer.write_datetime(col, self.read_datetime(reader).await?);
            }
            TdsDataType::DateTim4 => {
                let daypart = read_sync_first!(reader, try_read_uint16, read_uint16);
                let timepart = read_sync_first!(reader, try_read_uint16, read_uint16);
                writer.write_smalldatetime(
                    col,
                    SqlSmallDateTime {
                        days: daypart,
                        time: timepart,
                    },
                );
            }
            TdsDataType::DateTimeN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                match length {
                    0 => writer.write_null(col),
                    4 => writer.write_smalldatetime(col, self.read_small_datetime(reader).await?),
                    _ => writer.write_datetime(col, self.read_datetime(reader).await?),
                }
            }
            TdsDataType::DateN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                if length == 0 {
                    writer.write_null(col);
                } else {
                    writer.write_date(col, Self::read_date(reader).await?);
                }
            }
            TdsDataType::TimeN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                if length == 0 {
                    writer.write_null(col);
                } else {
                    writer.write_time(
                        col,
                        self.read_time(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "TimeN type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await?,
                    );
                }
            }
            TdsDataType::DateTime2N => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                if length == 0 {
                    writer.write_null(col);
                } else {
                    let cv = self
                        .read_datetime2(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTime2N type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await?;
                    if let ColumnValues::DateTime2(dt2) = cv {
                        writer.write_datetime2(col, dt2);
                    }
                }
            }
            TdsDataType::DateTimeOffsetN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                if length == 0 {
                    writer.write_null(col);
                } else {
                    let cv = self
                        .read_datetime_offset(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTimeOffsetN type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await?;
                    if let ColumnValues::DateTimeOffset(dto) = cv {
                        writer.write_datetimeoffset(col, dto);
                    }
                }
            }

            // === GUID ===
            TdsDataType::Guid => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                if length == 0 {
                    writer.write_null(col);
                } else {
                    if length != 16 {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Invalid GUID length: expected 16 bytes, got {length}"
                        )));
                    }
                    let mut bytes = [0u8; 16];
                    reader.read_bytes(&mut bytes).await?;
                    let uuid = uuid::Uuid::from_slice_le(&bytes).map_err(|e| {
                        crate::error::Error::ProtocolError(format!("Failed to parse UUID: {e}"))
                    })?;
                    writer.write_uuid(col, uuid);
                }
            }

            // A variant carries its own base type, which the caller may need in
            // order to report the underlying type; the decoded value cannot
            // always recover it.
            TdsDataType::SsVariant => {
                let (base, value) = self.read_sql_variant_with_base(reader).await?;
                // A NULL variant has no base type to report.
                if let Some(base) = base {
                    writer.write_variant_base_type(col, base);
                }
                write_column_value(writer, col, value);
            }

            // === Fallback: rare types go through decode() → write_column_value() ===
            _ => {
                let value = self.decode(reader, metadata).await?;
                write_column_value(writer, col, value);
            }
        }
        Ok(())
    }
}

/// How a PLP value's payload is framed, read from its 8-byte length prefix.
enum PlpFraming {
    /// `SQL_PLP_NULL` — the value is NULL and has no payload.
    Null,
    /// The payload length is declared up front.
    Known(usize),
    /// `SQL_PLP_UNKNOWNLEN` — length is only known once the chunks terminate.
    Unknown,
}

impl SqlTypeDecode for GenericDecoder {
    async fn decode<T>(&self, reader: &mut T, metadata: &ColumnMetadata) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let result = match metadata.data_type {
            TdsDataType::Int1 => {
                let value = read_sync_first!(reader, try_read_byte, read_byte);
                ColumnValues::from(value)
            }
            TdsDataType::Int2 => {
                let value = read_sync_first!(reader, try_read_int16, read_int16);
                ColumnValues::SmallInt(value)
            }
            TdsDataType::Int4 => {
                let value = read_sync_first!(reader, try_read_int32, read_int32);
                ColumnValues::from(value)
            }
            TdsDataType::Int8 => {
                let value = read_sync_first!(reader, try_read_int64, read_int64);
                ColumnValues::BigInt(value)
            }
            TdsDataType::Flt4 => {
                let value = read_sync_first!(reader, try_read_float32, read_float32);
                ColumnValues::Real(value)
            }
            TdsDataType::Flt8 => {
                let value = read_sync_first!(reader, try_read_float64, read_float64);
                ColumnValues::Float(value)
            }
            TdsDataType::Money4 => ColumnValues::SmallMoney(self.read_money4(reader).await?),
            TdsDataType::Money => ColumnValues::Money(self.read_money8(reader).await?),
            TdsDataType::MoneyN => {
                let byte_len = read_sync_first!(reader, try_read_byte, read_byte);
                match byte_len {
                    4 => ColumnValues::SmallMoney(self.read_money4(reader).await?),
                    8 => ColumnValues::Money(self.read_money8(reader).await?),
                    0 => ColumnValues::Null,
                    _ => {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Invalid MoneyN length - {byte_len}"
                        )));
                    }
                }
            }
            TdsDataType::DecimalN => {
                let value = self.read_decimal(reader, metadata).await?;
                match value {
                    Some(value) => ColumnValues::Decimal(value),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::NumericN => {
                let value = self.read_decimal(reader, metadata).await?;
                match value {
                    Some(value) => ColumnValues::Numeric(value),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::Bit => {
                let value = read_sync_first!(reader, try_read_byte, read_byte);
                ColumnValues::Bit(value == 1)
            }
            TdsDataType::NChar
            | TdsDataType::NVarChar
            | TdsDataType::BigChar
            | TdsDataType::BigVarChar
            | TdsDataType::Char
            | TdsDataType::VarChar
            | TdsDataType::NText
            | TdsDataType::Text => self.string_decoder.decode(reader, metadata).await?,
            TdsDataType::DateTime => {
                let value = self.read_datetime(reader).await?;
                ColumnValues::DateTime(value)
            }
            TdsDataType::IntN => {
                let byte_len = read_sync_first!(reader, try_read_byte, read_byte);
                self.read_intn(reader, byte_len).await?
            }
            TdsDataType::BigBinary => {
                let length = read_sync_first!(reader, try_read_uint16, read_uint16);
                // 0xFFFF is the USHORTLEN NULL marker (CHARBIN_NULL).
                if length == 0xFFFF {
                    ColumnValues::Null
                } else {
                    if length as usize > MAX_ALLOC_SIZE {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "BigBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                        )));
                    }
                    let mut bytes = vec![0u8; length as usize];
                    reader.read_bytes(&mut bytes).await?;
                    ColumnValues::Bytes(bytes)
                }
            }
            TdsDataType::BigVarBinary => {
                if metadata.is_plp() {
                    let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                    match some_bytes {
                        Some(bytes) => ColumnValues::Bytes(bytes),
                        None => ColumnValues::Null,
                    }
                } else {
                    let length = read_sync_first!(reader, try_read_uint16, read_uint16);
                    // 0xFFFF is the USHORTLEN NULL marker (CHARBIN_NULL).
                    if length == 0xFFFF {
                        ColumnValues::Null
                    } else {
                        if length as usize > MAX_ALLOC_SIZE {
                            return Err(crate::error::Error::ProtocolError(format!(
                                "BigVarBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                            )));
                        }
                        let mut bytes = vec![0u8; length as usize];
                        reader.read_bytes(&mut bytes).await?;
                        ColumnValues::Bytes(bytes)
                    }
                }
            }
            TdsDataType::Xml => {
                if !metadata.is_plp() {
                    return Err(crate::error::Error::ProtocolError(
                        "XML column metadata is not partially-length-prefixed".to_string(),
                    ));
                }
                let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                match some_bytes {
                    Some(bytes) => ColumnValues::Xml(SqlXml { bytes }),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::Json => {
                if !metadata.is_plp() {
                    return Err(crate::error::Error::ProtocolError(
                        "JSON column metadata is not partially-length-prefixed".to_string(),
                    ));
                }
                let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                match some_bytes {
                    Some(bytes) => ColumnValues::Json(SqlJson::new(bytes)),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::Vector => self.decode_vector(reader, metadata).await?,
            TdsDataType::BitN => {
                let byte_len = read_sync_first!(reader, try_read_byte, read_byte);
                if byte_len > 0 {
                    let value = read_sync_first!(reader, try_read_byte, read_byte);
                    ColumnValues::Bit(value == 1)
                } else {
                    ColumnValues::Null
                }
            }
            TdsDataType::Guid => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                Self::read_guid(reader, length).await?
            }
            TdsDataType::FltN => {
                // This is variable length float, hence the length needs to be read first
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                if length == 0 {
                    return Ok(ColumnValues::Null);
                }
                if length == 4 {
                    let value = read_sync_first!(reader, try_read_float32, read_float32);
                    ColumnValues::Real(value)
                } else {
                    let value = read_sync_first!(reader, try_read_float64, read_float64);
                    ColumnValues::Float(value)
                }
            }
            TdsDataType::DateTimeN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                // If length is 0, then it is NULL
                if length == 0 {
                    return Ok(ColumnValues::Null);
                } else if length == 4 {
                    // SmallDateTime
                    let smalldatetime = self.read_small_datetime(reader).await?;
                    return Ok(ColumnValues::SmallDateTime(smalldatetime));
                } else {
                    // DateTime
                    return Ok(ColumnValues::DateTime(self.read_datetime(reader).await?));
                }
            }
            TdsDataType::DateN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                return Self::read_daten(reader, length).await;
            }
            TdsDataType::TimeN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                match length {
                    0 => return Ok(ColumnValues::Null),
                    _ => {
                        return Ok(ColumnValues::Time(
                            self.read_time(
                                reader,
                                length,
                                metadata.get_scale().ok_or_else(|| {
                                    crate::error::Error::ImplementationError(
                                        "TimeN type should have scale".to_string(),
                                    )
                                })?,
                            )
                            .await?,
                        ));
                    }
                }
            }
            TdsDataType::DateTime2N => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                match length {
                    0 => Ok(ColumnValues::Null),
                    _ => {
                        self.read_datetime2(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTime2N type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await
                    }
                }
            }?,
            TdsDataType::DateTimeOffsetN => {
                let length = read_sync_first!(reader, try_read_byte, read_byte);
                match length {
                    0 => Ok(ColumnValues::Null),
                    _ => {
                        self.read_datetime_offset(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTimeOffsetN type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await
                    }
                }
            }?,
            TdsDataType::Image => {
                let text_ptr_len = read_sync_first!(reader, try_read_byte, read_byte) as usize;

                let length = if text_ptr_len > 0 {
                    const TIMESTAMP_BYTE_COUNT: usize = 8;
                    reader.skip_bytes(text_ptr_len).await?;
                    reader.skip_bytes(TIMESTAMP_BYTE_COUNT).await?;
                    read_sync_first!(reader, try_read_uint32, read_uint32) as usize
                } else {
                    0
                };

                if length == 0 {
                    ColumnValues::Null
                } else {
                    if length > MAX_ALLOC_SIZE {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Image length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                        )));
                    }
                    let mut buffer = vec![0u8; length];
                    reader.read_bytes(&mut buffer).await?;
                    ColumnValues::Bytes(buffer)
                }
            }
            TdsDataType::Udt => {
                if !metadata.is_plp() {
                    return Err(crate::error::Error::ProtocolError(
                        "UDT column metadata is not partially-length-prefixed".to_string(),
                    ));
                }
                let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                match some_bytes {
                    Some(bytes) => ColumnValues::Bytes(bytes),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::SsVariant => self.read_sql_variant(reader).await?,
            TdsDataType::DateTim4 => {
                let daypart = read_sync_first!(reader, try_read_uint16, read_uint16);
                let timepart = read_sync_first!(reader, try_read_uint16, read_uint16);
                ColumnValues::SmallDateTime(SqlSmallDateTime {
                    days: daypart,
                    time: timepart,
                })
            }
            TdsDataType::Decimal => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "Fixed-length Decimal type".to_string(),
                    context: format!(
                        "Data type {:?} (0x{:02X}) is not implemented. Use DecimalN instead.",
                        metadata.data_type, metadata.data_type as u8
                    ),
                });
            }
            TdsDataType::Numeric => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "Fixed-length Numeric type".to_string(),
                    context: format!(
                        "Data type {:?} (0x{:02X}) is not implemented. Use NumericN instead.",
                        metadata.data_type, metadata.data_type as u8
                    ),
                });
            }
            _ => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: format!("Data type {:?}", metadata.data_type),
                    context: format!(
                        "Data type {:?} (0x{:02X}) is not yet supported in the decoder",
                        metadata.data_type, metadata.data_type as u8
                    ),
                });
            }
        };
        Ok(result)
    }
}

#[derive(Debug, Default)]
struct StringDecoder;

impl StringDecoder {
    fn is_long_len_type(data_type: TdsDataType) -> bool {
        matches!(data_type, TdsDataType::NText | TdsDataType::Text)
    }

    async fn decode_string_into<T, W>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
        col: usize,
        writer: &mut W,
    ) -> TdsResult<()>
    where
        T: TdsPacketReader + Send + Sync,
        W: RowWriter + ?Sized,
    {
        let encoding_type = get_encoding_type(metadata);

        if metadata.is_plp() {
            match GenericDecoder::read_plp_framing(reader).await? {
                PlpFraming::Null => writer.write_null(col),
                PlpFraming::Known(length) => {
                    read_value_into!(
                        writer,
                        col,
                        ValueKind::String(&encoding_type),
                        length,
                        |dest| GenericDecoder::read_plp_chunks_into_slice(reader, dest, length)
                            .await,
                        |bytes| writer.write_string(col, SqlString::new(bytes, encoding_type)),
                    );
                }
                PlpFraming::Unknown => {
                    let bytes = GenericDecoder::read_plp_chunks_unknown_len(reader).await?;
                    writer.write_string(col, SqlString::new(bytes, encoding_type));
                }
            }
        } else if Self::is_long_len_type(metadata.data_type) {
            let text_ptr_len = read_sync_first!(reader, try_read_byte, read_byte) as usize;

            if text_ptr_len == 0 {
                writer.write_null(col);
                return Ok(());
            }

            const TIMESTAMP_BYTE_COUNT: usize = 8;
            reader.skip_bytes(text_ptr_len).await?;
            reader.skip_bytes(TIMESTAMP_BYTE_COUNT).await?;
            let length = read_sync_first!(reader, try_read_uint32, read_uint32) as usize;

            if length > MAX_ALLOC_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Text data length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                )));
            }

            let sql_string = if length == 0 {
                SqlString::new(Vec::new(), encoding_type)
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;
                SqlString::new(buffer, encoding_type)
            };
            writer.write_string(col, sql_string);
        } else {
            let length = read_sync_first!(reader, try_read_uint16, read_uint16) as usize;
            if length == 0xFFFF {
                writer.write_null(col);
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;
                writer.write_string(col, SqlString::new(buffer, encoding_type));
            }
        }
        Ok(())
    }
}

impl SqlTypeDecode for StringDecoder {
    async fn decode<T>(&self, reader: &mut T, metadata: &ColumnMetadata) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let encoding_type = get_encoding_type(metadata);

        // If Plp Column. (BIGVARCHARTYPE, BIGVARBINARYTYPE, NVARCHARTYPE with md.length == ushort.max)
        if metadata.is_plp() {
            let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
            match some_bytes {
                Some(bytes) => Ok(ColumnValues::String(SqlString::new(bytes, encoding_type))),
                None => Ok(ColumnValues::Null),
            }
        } else if Self::is_long_len_type(metadata.data_type) {
            // Legacy LOB types (TEXT/NTEXT/IMAGE) reading implementation
            //
            // WIRE FORMAT (from .NET TdsParser.cs:6517-6600):
            // 1. textptr_len (1 byte): Length of text pointer
            //    - 0x00 = NULL value
            //    - 0x10 (16) = Valid pointer (typical)
            // 2. textptr (textptr_len bytes): Text pointer (usually 16 bytes)
            //    - Server-managed pointer, client treats as opaque
            // 3. timestamp (8 bytes): Row timestamp
            //    - Used for optimistic concurrency
            // 4. data_length (4 bytes, uint32): Actual data length in bytes
            //    - For NTEXT: byte count (divide by 2 for char count)
            //    - For TEXT: byte count in the collation's encoding
            // 5. data (data_length bytes): The actual string data
            //    - For NTEXT: UTF-16LE encoded
            //    - For TEXT: encoded per collation (LCID-based)
            //
            // CURRENT IMPLEMENTATION STATUS:
            // Reads textptr_len (1 byte)
            // Skips textptr (16 bytes) and timestamp (8 bytes)
            // Reads data_length (4 bytes, uint32)
            // Allocates buffer and reads data
            // Creates SqlString with appropriate encoding type
            // NULL handling works (textptr_len = 0)
            // LCID-based decoding implemented (see sql_string.rs)
            let text_ptr_len = read_sync_first!(reader, try_read_byte, read_byte) as usize;

            let length = if text_ptr_len > 0 {
                const TIMESTAMP_BYTE_COUNT: usize = 8;
                reader.skip_bytes(text_ptr_len).await?;
                reader.skip_bytes(TIMESTAMP_BYTE_COUNT).await?;
                read_sync_first!(reader, try_read_uint32, read_uint32) as usize
            } else {
                // text_ptr_len == 0 means NULL value
                return Ok(ColumnValues::Null);
            };

            // Empty string (length == 0 but textptr_len > 0) is valid - return empty string, not NULL
            if length > MAX_ALLOC_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Text data length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                )));
            }

            let sql_string = if length == 0 {
                // Create empty SqlString with appropriate encoding
                SqlString::new(Vec::new(), encoding_type)
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;
                SqlString::new(buffer, encoding_type)
            };
            Ok(ColumnValues::String(sql_string))
        } else {
            let length = read_sync_first!(reader, try_read_uint16, read_uint16) as usize;
            if length == 0xFFFF {
                Ok(ColumnValues::Null)
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;

                let sql_string = SqlString::new(buffer, encoding_type);

                Ok(ColumnValues::String(sql_string))
            }
        }
    }
}

/// Bytes needed to render any `decimal`/`numeric`: sign, 38 digits, decimal
/// point, and slack for a scale that exceeds the digit count (`0.000…`).
pub const DECIMAL_STR_LEN: usize = 48;
const _: () = assert!(
    DECIMAL_STR_LEN >= 42,
    "buffer must hold 39 digits of u128::MAX plus a decimal point and sign"
);

/// TDS representation of Decimal and Numeric types.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DecimalParts {
    /// `true` for non-negative values.
    pub is_positive: bool,
    /// Number of digits after the decimal point.
    pub scale: u8,
    /// Total number of significant digits.
    pub precision: u8,
    /// The magnitude, as two 64-bit halves, least significant first.
    ///
    /// Split rather than held as a `u128` because a `u128` field would give
    /// `DecimalParts` — and through it `ColumnValues` — 16-byte alignment,
    /// which propagates into the row-fetch async state machines and grows them
    /// by ~4%. As two halves the struct is 24 bytes at align 8, against 32 at
    /// align 16; [`Self::magnitude`] recombines them with a shift and an or.
    magnitude: [u64; 2],
}

impl fmt::Display for DecimalParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.format_into(&mut [0u8; DECIMAL_STR_LEN]))
    }
}

impl DecimalParts {
    fn clamped_scale(&self) -> usize {
        (self.scale as usize).min(DECIMAL_STR_LEN - 3)
    }

    /// Builds a value from its unsigned magnitude, already scaled by `10^scale`.
    ///
    /// SQL Server's maximum precision of 38 digits keeps the magnitude below
    /// `10^38 < 2^127`, so every valid `decimal`/`numeric` fits.
    pub const fn new(is_positive: bool, precision: u8, scale: u8, magnitude: u128) -> Self {
        DecimalParts {
            is_positive,
            scale,
            precision,
            magnitude: [magnitude as u64, (magnitude >> 64) as u64],
        }
    }

    /// The unsigned magnitude, already scaled by `10^scale`.
    pub const fn magnitude(&self) -> u128 {
        (self.magnitude[0] as u128) | ((self.magnitude[1] as u128) << 64)
    }

    /// Create a DecimalParts from a decimal string using BigDecimal.
    ///
    /// Supports SQL Server's full 38-digit precision using arbitrary-precision arithmetic.
    /// More efficient and robust than manual parsing.
    ///
    /// # Arguments
    /// * `s` - String like "123.45", "-0.01", "99999999999999999999999999999999999999"
    /// * `precision` - Total number of digits (1-38 for SQL Server)
    /// * `scale` - Number of digits after decimal point (0-precision)
    ///
    /// # Returns
    /// DecimalParts or Error if parsing fails or precision/scale validation fails
    pub fn from_string(s: &str, precision: u8, scale: u8) -> TdsResult<Self> {
        use bigdecimal::num_bigint::{BigInt, Sign};
        use bigdecimal::{BigDecimal, Zero};
        use std::str::FromStr;

        let trimmed = s.trim();

        // Check for negative zero in the original string before parsing
        // BigDecimal normalizes -0 to 0, so we need to detect it early
        let has_negative_sign = trimmed.starts_with('-');

        // Parse the string into a BigDecimal
        let decimal = BigDecimal::from_str(trimmed).map_err(|e| {
            crate::error::Error::TypeConversionError(format!(
                "Invalid decimal string '{}': {}",
                s, e
            ))
        })?;

        // Check if input has more fractional digits than target scale
        // Get the scale of the input decimal
        let input_scale = decimal.fractional_digit_count();
        if input_scale > scale as i64 {
            return Err(crate::error::Error::TypeConversionError(format!(
                "Input decimal scale {} exceeds target scale {}",
                input_scale, scale
            )));
        }

        // Handle zero case (but preserve sign from original string)
        if decimal.is_zero() {
            return Ok(Self::new(!has_negative_sign, precision, scale, 0));
        }

        // Extract sign for non-zero values
        let is_positive = decimal.sign() != Sign::Minus;
        let abs_decimal = decimal.abs();

        // Scale the decimal: multiply by 10^scale to shift to integer representation
        let scale_factor = BigDecimal::from(10u64).powi(scale as i64);
        let scaled = abs_decimal * scale_factor;

        // Round to nearest integer
        let rounded = scaled.round(0);

        // Extract as BigInt, handling any remaining exponent
        let (bigint, exponent) = rounded.into_bigint_and_exponent();
        let final_bigint = if exponent > 0 {
            bigint * BigInt::from(10u64).pow(exponent as u32)
        } else if exponent < 0 {
            bigint / BigInt::from(10u64).pow((-exponent) as u32)
        } else {
            bigint
        };

        // Validate precision
        let digits_str = final_bigint.to_string();
        if digits_str.len() > precision as usize {
            return Err(crate::error::Error::TypeConversionError(format!(
                "Decimal value has {} digits, exceeds target precision {}",
                digits_str.len(),
                precision
            )));
        }

        // Convert BigInt to the little-endian 128-bit magnitude. A precision <= 38
        // value never needs more than 16 bytes; a caller that declares a wider
        // precision is refused rather than silently truncated.
        let bytes = final_bigint.to_signed_bytes_le();
        if bytes.len() > DECIMAL_MAGNITUDE_BYTES {
            return Err(crate::error::Error::TypeConversionError(format!(
                "Decimal value '{s}' does not fit in a 128-bit magnitude"
            )));
        }
        let mut magnitude_bytes = [0u8; DECIMAL_MAGNITUDE_BYTES];
        magnitude_bytes[..bytes.len()].copy_from_slice(&bytes);

        Ok(Self::new(
            is_positive,
            precision,
            scale,
            u128::from_le_bytes(magnitude_bytes),
        ))
    }

    /// Create a DecimalParts from an i64 value.
    pub fn from_i64(value: i64, precision: u8, scale: u8) -> TdsResult<Self> {
        let is_positive = value >= 0;

        let magnitude = 10u128
            .checked_pow(u32::from(scale))
            .and_then(|factor| u128::from(value.unsigned_abs()).checked_mul(factor))
            .ok_or_else(|| {
                crate::error::Error::TypeConversionError(format!(
                    "Decimal value {value} scaled by 10^{scale} does not fit in a 128-bit magnitude"
                ))
            })?;

        Ok(Self::new(is_positive, precision, scale, magnitude))
    }

    /// Create a DecimalParts from an f64 value.
    pub fn from_f64(value: f64, precision: u8, scale: u8) -> TdsResult<Self> {
        // Convert f64 to string with appropriate precision
        let s = format!("{:.prec$}", value, prec = scale as usize);
        Self::from_string(&s, precision, scale)
    }

    /// Renders the value into `buf` and returns the populated suffix, without
    /// allocating. The output is the same text [`Display`](fmt::Display)
    /// produces: `"123.45"`, `"-0.01"`, `"0.000"`.
    ///
    /// Digits are emitted least significant first, so a caller-owned buffer
    /// avoids the intermediate `String` the sign and decimal point would
    /// otherwise need.
    pub fn format_into<'a>(&self, buf: &'a mut [u8; DECIMAL_STR_LEN]) -> &'a str {
        // Every digit below is produced with `u64` arithmetic: a `u128 % 10`
        // loop emits a `__udivti3` libcall per digit and loses to
        // `u128::to_string` on wide values. Splitting into limbs of `10^19`
        // (the largest power of ten a `u64` holds) costs at most two `u128`
        // divisions, and none at all when the magnitude already fits a `u64`.
        // Three limbs are needed, not two: a `u128` reaches 39 digits.
        const LIMB: u128 = 10_000_000_000_000_000_000;
        const LIMB_DIGITS: usize = 19;

        let magnitude = self.magnitude();
        let mut limbs = [0u64; 3];
        let limb_count;
        if magnitude <= u64::MAX as u128 {
            limbs[0] = magnitude as u64;
            limb_count = 1;
        } else {
            limbs[0] = (magnitude % LIMB) as u64;
            let rest = magnitude / LIMB;
            if rest <= u64::MAX as u128 {
                limbs[1] = rest as u64;
                limb_count = 2;
            } else {
                limbs[1] = (rest % LIMB) as u64;
                limbs[2] = (rest / LIMB) as u64;
                limb_count = 3;
            }
        }

        // `scale` is wire-supplied and unvalidated. A `decimal` never exceeds
        // 38, but the loop is bounded by the buffer rather than by trusting it,
        // so a malformed value renders short instead of overrunning.
        let scale = self.clamped_scale();
        let mut pos = buf.len();
        let mut emitted = 0usize;
        let mut limb = 0usize;
        let mut taken = 0usize;
        let mut current = limbs[0];

        loop {
            if scale > 0 && emitted == scale {
                pos -= 1;
                buf[pos] = b'.';
            }

            let digit = (current % 10) as u8;
            current /= 10;
            taken += 1;
            pos -= 1;
            buf[pos] = b'0' + digit;
            emitted += 1;

            // A limb always contributes all 19 of its digits, zeros included,
            // while a more significant one is still to come.
            if taken == LIMB_DIGITS && limb + 1 < limb_count {
                limb += 1;
                current = limbs[limb];
                taken = 0;
            }

            // Leading zeros are otherwise only emitted to fill the fractional
            // part, so a value narrower than its scale renders as `0.00…`.
            if limb + 1 == limb_count && current == 0 && emitted > scale {
                break;
            }
        }

        if !self.is_positive {
            pos -= 1;
            buf[pos] = b'-';
        }

        // Every byte written above is ASCII.
        core::str::from_utf8(&buf[pos..]).unwrap_or_default()
    }

    /// Renders the value into a new `String`.
    ///
    /// Equivalent to [`ToString::to_string`], but it copies a known-length
    /// `&str` instead of going through [`fmt::Formatter`], so it costs one
    /// allocation and nothing else. Callers that can supply their own buffer
    /// should use [`Self::format_into`] and pay nothing.
    pub fn to_decimal_string(&self) -> String {
        self.format_into(&mut [0u8; DECIMAL_STR_LEN]).to_owned()
    }

    fn to_f64(self) -> f64 {
        let magnitude = self.magnitude() as f64;

        let d_ret = magnitude / 10.0_f64.powi(self.clamped_scale() as i32);

        if self.is_positive { d_ret } else { -d_ret }
    }

    /// The `index`-th little-endian 32-bit word of the magnitude, counting from
    /// the least significant. Words past the fourth are always zero.
    ///
    /// This is the form the TDS wire and every reference driver's normalized
    /// decimal use.
    pub const fn word(&self, index: usize) -> i32 {
        if index >= MAX_DECIMAL_INT_PARTS {
            return 0;
        }
        (self.magnitude() >> (index * 32)) as u32 as i32
    }

    /// Number of little-endian 32-bit words needed to hold the magnitude, at
    /// least one so that zero still has a word.
    ///
    /// This is the minimal count, not the width the value was decoded from:
    /// trailing zero words are not reported. Consumers must accumulate the
    /// returned words rather than assuming a fixed width.
    pub fn word_count(&self) -> usize {
        (128 - self.magnitude().leading_zeros() as usize)
            .div_ceil(32)
            .max(1)
    }

    /// Builds a value from little-endian 32-bit words, least significant first.
    ///
    /// Zero words past the fourth are ignored. Non-zero words cannot fit in the
    /// representation and trigger a debug assertion; callers at trust boundaries
    /// should use [`Self::try_from_words`] to reject them in every build.
    pub fn from_words(is_positive: bool, precision: u8, scale: u8, words: &[i32]) -> Self {
        debug_assert!(
            words
                .iter()
                .skip(MAX_DECIMAL_INT_PARTS)
                .all(|&word| word == 0),
            "magnitude wider than 128 bits truncated: {words:?}"
        );

        let magnitude = words
            .iter()
            .take(MAX_DECIMAL_INT_PARTS)
            .enumerate()
            .fold(0u128, |acc, (i, &word)| {
                acc | ((word as u32 as u128) << (i * 32))
            });

        Self::new(is_positive, precision, scale, magnitude)
    }

    /// Builds a value from little-endian 32-bit words, rejecting magnitudes
    /// wider than 128 bits while accepting trailing zero padding.
    pub fn try_from_words(
        is_positive: bool,
        precision: u8,
        scale: u8,
        words: &[i32],
    ) -> Option<Self> {
        words
            .iter()
            .skip(MAX_DECIMAL_INT_PARTS)
            .all(|&word| word == 0)
            .then(|| Self::from_words(is_positive, precision, scale, words))
    }
}

impl Debug for DecimalParts {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Decimal: {} Precision {} Scale {} F64 value: {}",
            self,
            self.precision,
            self.scale,
            self.to_f64()
        )
    }
}

async fn decode_two_propbyte_variant<T>(
    reader: &mut T,
    variant_base_type: u8,
    tds_type: TdsDataType,
    data_length: u32,
) -> TdsResult<ColumnValues>
where
    T: TdsPacketReader + Send + Sync,
{
    Ok(match tds_type {
        // BIGVARBINARYTYPE, BIGBINARYTYPE
        TdsDataType::BigVarBinary | TdsDataType::BigBinary => {
            let _max_length: u16 = read_sync_first!(reader, try_read_uint16, read_uint16);
            if data_length as usize > MAX_ALLOC_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "SQL Variant binary data length {data_length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                )));
            }
            let mut buffer = vec![0u8; data_length as usize];
            reader.read_bytes(&mut buffer).await?;
            ColumnValues::Bytes(buffer)
        }
        TdsDataType::NumericN | TdsDataType::DecimalN => {
            let precision = read_sync_first!(reader, try_read_byte, read_byte);
            let scale = read_sync_first!(reader, try_read_byte, read_byte);
            let decimal_parts =
                GenericDecoder::read_decimal_data(reader, data_length as u8, precision, scale)
                    .await?;

            if matches!(tds_type, TdsDataType::NumericN) {
                match decimal_parts {
                    Some(value) => ColumnValues::Numeric(value),
                    None => ColumnValues::Null,
                }
            } else {
                match decimal_parts {
                    Some(value) => ColumnValues::Decimal(value),
                    None => ColumnValues::Null,
                }
            }
        }
        _ => {
            return Err(crate::error::Error::ProtocolError(format!(
                "Unexpected SQL variant base type for len(2) prop bytes: {variant_base_type:#04X}. Expected binary or numeric types."
            )));
        }
    })
}

async fn decode_seven_propbyte_variant<T>(
    reader: &mut T,
    tds_type: TdsDataType,
    data_length: u32,
) -> TdsResult<ColumnValues>
where
    T: TdsPacketReader + Send + Sync,
{
    if !matches!(
        tds_type,
        TdsDataType::BigVarChar | TdsDataType::BigChar | TdsDataType::NVarChar | TdsDataType::NChar
    ) {
        return Err(crate::error::Error::ProtocolError(format!(
            "Unexpected SQL variant base type for len(7) prop bytes: {tds_type:?}. Expected a character type."
        )));
    }
    let mut collation_bytes = vec![0u8; 5];
    reader.read_bytes(&mut collation_bytes).await?;
    let _max_length = read_sync_first!(reader, try_read_uint16, read_uint16) as usize;
    let collation: SqlCollation = collation_bytes.as_slice().try_into()?;
    if data_length as usize > MAX_ALLOC_SIZE {
        return Err(crate::error::Error::ProtocolError(format!(
            "SQL Variant string data length {data_length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
        )));
    }
    let mut buffer = vec![0u8; data_length as usize];
    reader.read_bytes(&mut buffer).await?;
    let encoding = if matches!(tds_type, TdsDataType::NVarChar | TdsDataType::NChar) {
        EncodingType::Utf16
    } else if collation.utf8() {
        EncodingType::Utf8
    } else {
        EncodingType::LcidBased(collation)
    };
    let sql_string = SqlString::new(buffer, encoding);
    Ok(ColumnValues::String(sql_string))
}

#[cfg(test)]
mod test {
    use crate::datatypes::{
        column_values::ColumnValues,
        decoder::{
            DecimalParts, GenericDecoder, MAX_ALLOC_SIZE, StringDecoder, validate_alloc_size,
        },
        sqldatatypes::TdsDataType,
    };

    #[test]
    fn test_f64_conversion() {
        let expected: f64 = 123456.322;

        // Represents 123456.322 as observed over TDS wire.
        let magnitude = 12345632200;
        let parts = DecimalParts::new(true, 18, 5, magnitude);

        assert_eq!(expected, parts.to_f64());
    }

    #[test]
    fn test_f64_conversion_negative() {
        let expected: f64 = -123456.322;

        // Represents -123456.322 as observed over TDS wire.
        let magnitude = 12345632200;
        let parts = DecimalParts::new(false, 18, 5, magnitude);

        assert_eq!(expected, parts.to_f64());
    }

    #[test]
    fn test_f64_conversion_zero() {
        let expected: f64 = 0.0;

        let magnitude = 0;
        let parts = DecimalParts::new(true, 1, 0, magnitude);

        assert_eq!(expected, parts.to_f64());
    }

    #[test]
    fn test_f64_conversion_large_number() {
        // Test conversion with larger numbers
        let magnitude = 100000;
        let parts = DecimalParts::new(true, 7, 2, magnitude);

        let result = parts.to_f64();
        // With scale of 2, int value 100000 should become 1000.00
        assert!((result - 1000.0).abs() < 0.01);
    }

    #[test]
    fn test_decimal_parts_with_multi_word_magnitude() {
        // Test with multiple integer parts to ensure full conversion
        let magnitude = 5294967296;
        let parts = DecimalParts::new(true, 19, 0, magnitude);

        // Should successfully convert to f64
        let result = parts.to_f64();
        assert!(result > 0.0);
    }

    #[test]
    fn test_u8_to_column_values() {
        let value: u8 = 123;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::TinyInt(v) => assert_eq!(v, 123),
            _ => panic!("Expected TinyInt variant"),
        }
    }

    #[test]
    fn test_i32_to_column_values() {
        let value: i32 = 12345;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, 12345),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_i32_negative_to_column_values() {
        let value: i32 = -12345;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, -12345),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_generic_decoder_default() {
        let _decoder = GenericDecoder::default();
    }

    #[test]
    fn test_string_decoder_default() {
        let _decoder = StringDecoder;
    }

    #[test]
    fn test_decimal_parts_debug() {
        let parts = DecimalParts::new(true, 10, 2, 1958505087099);
        let debug_str = format!("{parts:?}");
        // Just verify the debug trait works - don't assert on exact format
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_generic_decoder_constants() {
        assert_eq!(GenericDecoder::SHORTLEN_MAXVALUE, 65535);
        assert_eq!(GenericDecoder::SQL_PLP_NULL, 0xffffffffffffffff);
        assert_eq!(GenericDecoder::SQL_PLP_UNKNOWNLEN, 0xfffffffffffffffe);
        assert_eq!(GenericDecoder::SQL_PLP_MAXLEN, 0xfffffffffffffffd);
    }

    #[test]
    fn test_decimal_parts_scale_precision() {
        let parts = DecimalParts::new(true, 18, 5, 100000);

        // Test that scale affects the decimal conversion
        let result = parts.to_f64();
        // With scale of 5, int value 100000 should become 1.00000
        assert!((result - 1.0).abs() < 0.00001);
    }

    #[test]
    fn test_decimal_parts_zero_magnitude() {
        let parts = DecimalParts::new(true, 1, 0, 0);

        let result = parts.to_f64();
        assert_eq!(result, 0.0);
    }

    #[test]
    fn test_decimal_parts_single_int_part() {
        let parts = DecimalParts::new(true, 5, 0, 12345);

        let result = parts.to_f64();
        assert_eq!(result, 12345.0);
    }

    #[test]
    fn test_column_values_from_u8_zero() {
        let value: u8 = 0;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::TinyInt(v) => assert_eq!(v, 0),
            _ => panic!("Expected TinyInt variant"),
        }
    }

    #[test]
    fn test_column_values_from_u8_max() {
        let value: u8 = 255;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::TinyInt(v) => assert_eq!(v, 255),
            _ => panic!("Expected TinyInt variant"),
        }
    }

    #[test]
    fn test_column_values_from_i32_zero() {
        let value: i32 = 0;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, 0),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_column_values_from_i32_max() {
        let value: i32 = i32::MAX;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, i32::MAX),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_column_values_from_i32_min() {
        let value: i32 = i32::MIN;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, i32::MIN),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_validate_alloc_size_within_limit() {
        let result = validate_alloc_size(1024, "test_allocation");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_alloc_size_at_limit() {
        let result = validate_alloc_size(MAX_ALLOC_SIZE, "test_at_limit");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_alloc_size_exceeds_limit() {
        let result = validate_alloc_size(MAX_ALLOC_SIZE + 1, "test_exceeds");
        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = format!("{e:?}");
            assert!(error_msg.contains("exceeds maximum allowed"));
        }
    }

    #[test]
    fn test_validate_alloc_size_zero() {
        let result = validate_alloc_size(0, "test_zero");
        assert!(result.is_ok());
    }

    #[test]
    fn test_decimal_parts_equality_same() {
        let parts1 = DecimalParts::new(true, 10, 2, 858993459300);
        let parts2 = DecimalParts::new(true, 10, 2, 858993459300);
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_sign() {
        let parts1 = DecimalParts::new(true, 10, 2, 100);
        let parts2 = DecimalParts::new(false, 10, 2, 100);
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_scale() {
        let parts1 = DecimalParts::new(true, 10, 2, 100);
        let parts2 = DecimalParts::new(true, 10, 3, 100);
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_precision() {
        let parts1 = DecimalParts::new(true, 10, 2, 100);
        let parts2 = DecimalParts::new(true, 12, 2, 100);
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_length_with_zeros() {
        let parts1 = DecimalParts::new(true, 10, 2, 100);
        let parts2 = DecimalParts::new(true, 10, 2, 100);
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_length_with_nonzeros() {
        let parts1 = DecimalParts::new(true, 10, 2, 858993459300);
        let parts2 = DecimalParts::new(true, 10, 2, 100);
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_debug_format_positive() {
        let parts = DecimalParts::new(true, 10, 2, 12345);
        let debug_str = format!("{parts:?}");
        assert!(debug_str.starts_with("Decimal: 123.45 "));
        assert!(debug_str.contains("Precision 10"));
        assert!(debug_str.contains("Scale 2"));
    }

    #[test]
    fn test_decimal_parts_debug_format_negative() {
        let parts = DecimalParts::new(false, 15, 3, 54321);
        let debug_str = format!("{parts:?}");
        assert!(debug_str.starts_with("Decimal: -54.321 "));
        assert!(debug_str.contains("Precision 15"));
        assert!(debug_str.contains("Scale 3"));
    }

    #[test]
    fn test_decimal_parts_debug_format_multi_word_magnitude() {
        // Three 32-bit words: 100 | 200 << 32 | 300 << 64.
        let parts = DecimalParts::new(true, 20, 0, 5534023222971858944100);
        let debug_str = format!("{parts:?}");
        assert!(debug_str.starts_with("Decimal: 5534023222971858944100 "));
    }

    #[test]
    fn test_f64_conversion_high_scale() {
        let magnitude = 12345;
        let parts = DecimalParts::new(true, 15, 10, magnitude);
        let result = parts.to_f64();
        // With scale of 10, 12345 should become 0.0000012345
        assert!((result - 0.0000012345).abs() < 0.0000000001);
    }

    #[test]
    fn test_f64_conversion_single_zero() {
        let parts = DecimalParts::new(true, 10, 5, 0);
        let result = parts.to_f64();
        assert_eq!(result, 0.0);
    }

    #[test]
    fn test_f64_conversion_negative_zero() {
        let parts = DecimalParts::new(false, 1, 0, 0);
        let result = parts.to_f64();
        assert_eq!(result, -0.0);
    }

    #[test]
    fn test_string_decoder_is_long_len_type_ntext() {
        assert!(StringDecoder::is_long_len_type(TdsDataType::NText));
    }

    #[test]
    fn test_string_decoder_is_long_len_type_text() {
        assert!(StringDecoder::is_long_len_type(TdsDataType::Text));
    }

    #[test]
    fn test_string_decoder_is_long_len_type_not_long() {
        assert!(!StringDecoder::is_long_len_type(TdsDataType::NVarChar));
        assert!(!StringDecoder::is_long_len_type(TdsDataType::BigVarChar));
        assert!(!StringDecoder::is_long_len_type(TdsDataType::Int4));
    }

    #[test]
    fn test_decimal_parts_f64_conversion_with_wide_magnitude() {
        // Test with 3 int parts
        let parts = DecimalParts::new(true, 30, 0, 55340232229718589441);
        let result = parts.to_f64();
        // Just verify it doesn't panic and produces a value
        assert!(result > 0.0);
    }

    #[test]
    fn test_decimal_parts_equality_reversed_order() {
        // Test that order matters for trailing zeros
        let parts1 = DecimalParts::new(true, 10, 2, 100);
        let parts2 = DecimalParts::new(true, 10, 2, 429496729600);
        // These should not be equal as trailing zeros are in different positions
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_both_empty() {
        let parts1 = DecimalParts::new(true, 1, 0, 0);
        let parts2 = DecimalParts::new(true, 1, 0, 0);
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_validate_alloc_size_mid_range() {
        // Test allocation in the middle range
        let result = validate_alloc_size(MAX_ALLOC_SIZE / 2, "test_mid_range");
        assert!(result.is_ok());
    }

    #[test]
    fn test_decimal_parts_f64_negative_with_scale() {
        // Test negative number with scale
        let parts = DecimalParts::new(false, 10, 3, 123456);
        let result = parts.to_f64();
        assert!((result + 123.456).abs() < 0.001);
    }

    #[test]
    fn test_decimal_parts_equality_one_empty_one_zero() {
        // Test equality between empty vec and vec with zero
        let parts1 = DecimalParts::new(true, 1, 0, 0);
        let parts2 = DecimalParts::new(true, 1, 0, 0);
        // Empty should equal to [0]
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_debug_with_zero() {
        // Test Debug formatting with zero value
        let parts = DecimalParts::new(true, 1, 0, 0);
        let debug_str = format!("{parts:?}");
        assert!(debug_str.contains("0"));
        assert!(debug_str.contains("F64 value: 0"));
    }

    // Tests for DecimalParts::from_string
    #[test]
    fn test_from_string_positive_decimal() {
        let result = DecimalParts::from_string("123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(parts.is_positive);
        assert_eq!(parts.scale, 2);
        assert_eq!(parts.precision, 10);
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    #[test]
    fn test_from_string_negative_decimal() {
        let result = DecimalParts::from_string("-123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(!parts.is_positive);
        assert_eq!(parts.scale, 2);
        assert_eq!(parts.precision, 10);
        assert_eq!(parts.to_decimal_string(), "-123.45");
    }

    #[test]
    fn test_from_string_integer_no_decimal_point() {
        let result = DecimalParts::from_string("12345", 10, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(parts.is_positive);
        assert_eq!(parts.scale, 0);
        assert_eq!(parts.to_decimal_string(), "12345");
    }

    #[test]
    fn test_from_string_with_leading_zeros() {
        let result = DecimalParts::from_string("00123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    #[test]
    fn test_from_string_small_fractional_value() {
        let result = DecimalParts::from_string("0.01", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0.01");
    }

    #[test]
    fn test_from_string_zero() {
        let result = DecimalParts::from_string("0", 10, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0");
    }

    #[test]
    fn test_from_string_zero_with_scale() {
        let result = DecimalParts::from_string("0.00", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0.00");
    }

    #[test]
    fn test_from_string_fractional_padding() {
        // "1.5" with scale 3 should be treated as "1.500"
        let result = DecimalParts::from_string("1.5", 10, 3);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "1.500");
    }

    #[test]
    fn test_from_string_max_precision_38_digits() {
        let value = "12345678901234567890123456789012345678";
        let result = DecimalParts::from_string(value, 38, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), value);
    }

    #[test]
    fn test_from_string_high_scale() {
        let result = DecimalParts::from_string("123.456789", 10, 6);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.456789");
    }

    #[test]
    fn test_from_string_leading_zeros_precision_check() {
        // "00001.00" should have precision of 3 (1 significant digit + 2 scale)
        let result = DecimalParts::from_string("00001.00", 5, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "1.00");
    }

    #[test]
    fn test_from_string_with_positive_sign() {
        let result = DecimalParts::from_string("+123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(parts.is_positive);
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    // Error cases
    #[test]
    fn test_from_string_invalid_characters() {
        let result = DecimalParts::from_string("not_a_number", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("invalid digit"));
    }

    #[test]
    fn test_from_string_multiple_decimal_points() {
        let result = DecimalParts::from_string("123.45.67", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("Invalid decimal string"));
    }

    #[test]
    fn test_from_string_scale_exceeded() {
        // Trying to parse "123.456" with scale 2 should fail
        let result = DecimalParts::from_string("123.456", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("scale") && error_msg.contains("exceeds"));
    }

    #[test]
    fn test_from_string_precision_exceeded() {
        // "12345" has 5 significant digits, should fail with precision 4
        let result = DecimalParts::from_string("12345", 4, 0);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("precision") && error_msg.contains("exceeds"));
    }

    #[test]
    fn test_from_string_precision_exceeded_with_decimal() {
        // "123.45" has 5 significant digits total, should fail with precision 4
        let result = DecimalParts::from_string("123.45", 4, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("precision") && error_msg.contains("exceeds"));
    }

    #[test]
    fn test_from_string_invalid_digit_in_integer_part() {
        let result = DecimalParts::from_string("12a34", 10, 0);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("invalid digit"));
    }

    #[test]
    fn test_from_string_invalid_digit_in_fractional_part() {
        let result = DecimalParts::from_string("123.4x5", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("invalid digit"));
    }

    #[test]
    fn test_from_string_leading_zeros_not_counted_in_precision() {
        // "0000123" should be treated as precision 3, not 7
        let result = DecimalParts::from_string("0000123", 3, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123");
    }

    #[test]
    fn test_from_string_whitespace_trimmed() {
        let result = DecimalParts::from_string("  123.45  ", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    #[test]
    fn test_from_string_negative_zero() {
        let result = DecimalParts::from_string("-0", 10, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(!parts.is_positive);
        assert_eq!(parts.to_decimal_string(), "-0");
    }

    #[test]
    fn test_from_string_only_zeros_with_decimal() {
        let result = DecimalParts::from_string("0.0", 10, 1);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0.0");
    }

    #[test]
    fn test_from_string_exact_precision_match() {
        // Test when actual precision exactly matches target precision
        let result = DecimalParts::from_string("12345", 5, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "12345");
    }

    #[test]
    fn test_from_string_exact_scale_match() {
        // Test when fractional digits exactly match scale
        let result = DecimalParts::from_string("123.45", 5, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    #[test]
    fn test_decimal_parts_max_width_magnitude() {
        // Four words of all-ones is the widest magnitude a u128 holds.
        let parts = DecimalParts::new(true, 38, 0, u128::MAX);
        assert_eq!(parts.to_decimal_string(), u128::MAX.to_string());
        assert_eq!(parts.word_count(), 4);
        assert_eq!(parts.word(3), -1);
    }

    #[test]
    fn test_decimal_parts_try_from_words_rejects_oversized_magnitude() {
        assert!(DecimalParts::try_from_words(true, 38, 0, &[1, 0, 0, 0, 1]).is_none());
    }

    #[test]
    fn test_decimal_parts_from_words_with_trailing_zero_words() {
        // Zero-padded words past the fourth carry no magnitude.
        let parts = DecimalParts::try_from_words(false, 38, 2, &[12345, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(parts.magnitude(), 12345);
        assert_eq!(parts.to_decimal_string(), "-123.45");
    }

    #[test]
    fn test_decimal_parts_empty_magnitude_is_zero() {
        let parts = DecimalParts::from_words(true, 38, 0, &[]);
        assert_eq!(parts.magnitude(), 0);
        assert_eq!(parts.word_count(), 1);
        assert_eq!(parts.to_decimal_string(), "0");
    }

    // Rendering equivalence against an independent oracle
    mod decimal_format_tests {
        use super::*;
        use crate::datatypes::decoder::DECIMAL_STR_LEN;
        use rand::{Rng, SeedableRng, rngs::StdRng};

        /// The pre-existing rendering algorithm, kept verbatim as the oracle.
        ///
        /// It shares no arithmetic with [`DecimalParts::format_into`]: digits
        /// come from `u128::to_string`, the point is spliced by index, and the
        /// sign is prepended. A formatter bug therefore cannot hide behind an
        /// identical bug in the oracle.
        fn reference(is_positive: bool, scale: u8, magnitude: u128) -> String {
            let value_str = magnitude.to_string();

            let result = if scale == 0 {
                value_str
            } else {
                let scale_pos = scale as usize;
                if value_str.len() <= scale_pos {
                    format!("0.{}{}", "0".repeat(scale_pos - value_str.len()), value_str)
                } else {
                    let split_pos = value_str.len() - scale_pos;
                    format!("{}.{}", &value_str[..split_pos], &value_str[split_pos..])
                }
            };

            if is_positive {
                result
            } else {
                format!("-{result}")
            }
        }

        #[track_caller]
        fn assert_matches_reference(is_positive: bool, precision: u8, scale: u8, magnitude: u128) {
            let parts = DecimalParts::new(is_positive, precision, scale, magnitude);
            let expected = reference(is_positive, scale, magnitude);

            let mut buf = [0u8; DECIMAL_STR_LEN];
            assert_eq!(
                parts.format_into(&mut buf),
                expected,
                "format_into({is_positive}, {scale}, {magnitude})"
            );
            assert_eq!(parts.to_decimal_string(), expected);
            assert_eq!(parts.to_string(), expected);
        }

        #[test]
        fn exhaustive_small_magnitudes_match_reference() {
            for magnitude in 0u128..=4096 {
                for scale in 0u8..=6 {
                    assert_matches_reference(true, 38, scale, magnitude);
                    assert_matches_reference(false, 38, scale, magnitude);
                }
            }
        }

        #[test]
        fn powers_of_ten_match_reference_at_every_scale() {
            // 10^k and 10^k - 1 straddle every digit-count change, including the
            // u64/u128 split the formatter pivots on.
            let mut boundaries = vec![0u128, u128::MAX];
            for k in 1..=38u32 {
                let power = 10u128.pow(k);
                boundaries.push(power);
                boundaries.push(power - 1);
            }
            boundaries.push(u64::MAX as u128);
            boundaries.push(u64::MAX as u128 + 1);
            boundaries.push(10_000_000_000_000_000_000u128);

            for &magnitude in &boundaries {
                // Every scale, so `s % 4 != 0` at maximum magnitude is covered:
                // that is the shape the deferred base-10000 export must not
                // pre-multiply, because 10^38 * 100 wraps a u128.
                for scale in 0..=38u8 {
                    assert_matches_reference(true, 38, scale, magnitude);
                    assert_matches_reference(false, 38, scale, magnitude);
                }
            }
        }

        #[test]
        fn randomized_magnitudes_match_reference() {
            let mut rng = StdRng::seed_from_u64(0x5EED_D3C1);
            for _ in 0..20_000 {
                // Uniform over widths, not over u128, so the one-, two-, three-
                // and four-word wire shapes are all exercised.
                let bits = rng.random_range(1..=128u32);
                let magnitude = if bits == 128 {
                    rng.random::<u128>()
                } else {
                    rng.random::<u128>() & ((1u128 << bits) - 1)
                };
                let scale = rng.random_range(0..=38u8);
                let precision = rng.random_range(1..=38u8);
                assert_matches_reference(rng.random(), precision, scale, magnitude);
            }
        }

        #[test]
        fn precision_one_to_thirty_eight_match_reference() {
            for precision in 1..=38u8 {
                // The largest magnitude the precision admits, and one below it.
                let max = 10u128.pow(precision as u32) - 1;
                for scale in 0..=precision {
                    assert_matches_reference(true, precision, scale, max);
                    assert_matches_reference(false, precision, scale, max);
                    assert_matches_reference(true, precision, scale, max / 10);
                }
            }
        }

        #[test]
        fn negative_zero_renders_with_sign() {
            let parts = DecimalParts::new(false, 18, 0, 0);
            assert_eq!(parts.to_string(), "-0");
            assert_eq!(DecimalParts::new(false, 18, 4, 0).to_string(), "-0.0000");
        }

        #[test]
        fn zero_with_scale_renders_padded() {
            let parts = DecimalParts::new(true, 18, 6, 0);
            assert_eq!(parts.to_string(), "0.000000");
        }

        #[test]
        fn magnitude_narrower_than_scale_is_zero_padded() {
            let parts = DecimalParts::new(true, 18, 3, 5);
            assert_eq!(parts.to_string(), "0.005");
        }

        #[test]
        fn out_of_domain_scale_does_not_overrun_the_buffer() {
            // `scale` comes off the wire unvalidated. No `decimal` reaches 255,
            // but the formatter must not index past its buffer if one claims to.
            let parts = DecimalParts::new(false, 38, u8::MAX, u128::MAX);
            let mut buf = [0u8; DECIMAL_STR_LEN];
            assert_eq!(parts.format_into(&mut buf).len(), DECIMAL_STR_LEN);
        }

        #[test]
        fn format_into_leaves_the_buffer_reusable() {
            let mut buf = [0u8; DECIMAL_STR_LEN];
            for (magnitude, scale, expected) in [
                (1u128, 0u8, "1"),
                (123456u128, 4u8, "12.3456"),
                (0u128, 0u8, "0"),
            ] {
                let parts = DecimalParts::new(true, 38, scale, magnitude);
                assert_eq!(parts.format_into(&mut buf), expected);
            }
        }
    }

    // Constructors that used to wrap silently
    mod decimal_constructor_tests {
        use super::*;

        #[test]
        fn from_i64_rejects_a_scale_that_overflows_the_magnitude() {
            // 10^38 fits, but i64::MAX * 10^38 does not. This used to wrap.
            assert!(DecimalParts::from_i64(1, 38, 38).is_ok());
            let err = DecimalParts::from_i64(i64::MAX, 38, 38).unwrap_err();
            assert!(matches!(err, crate::error::Error::TypeConversionError(_)));
            assert!(DecimalParts::from_i64(1, 38, 39).is_err());
        }

        #[test]
        fn from_i64_round_trips_through_the_formatter() {
            let parts = DecimalParts::from_i64(-12345, 18, 3).unwrap();
            assert_eq!(parts.magnitude(), 12_345_000);
            assert_eq!(parts.to_string(), "-12345.000");
        }

        #[test]
        fn from_string_rejects_a_magnitude_wider_than_128_bits() {
            // Only reachable by declaring a precision past SQL Server's 38.
            let wide = "1".repeat(40);
            assert!(DecimalParts::from_string(&wide, 40, 0).is_err());
        }

        #[test]
        fn from_string_accepts_the_widest_valid_decimal() {
            let max = "9".repeat(38);
            let parts = DecimalParts::from_string(&max, 38, 0).unwrap();
            assert_eq!(parts.magnitude(), 10u128.pow(38) - 1);
            assert_eq!(parts.to_string(), max);
        }
    }

    // Little-endian word view used by the wire encoders and the JS FFI
    mod decimal_word_tests {
        use super::*;

        #[test]
        fn words_round_trip_through_from_words() {
            for magnitude in [
                0u128,
                1,
                u32::MAX as u128,
                u32::MAX as u128 + 1,
                u64::MAX as u128,
                u64::MAX as u128 + 1,
                u128::MAX,
                12_345_678_901_234_567_890,
            ] {
                let parts = DecimalParts::new(false, 38, 7, magnitude);
                let words: Vec<i32> = (0..parts.word_count()).map(|i| parts.word(i)).collect();
                assert_eq!(DecimalParts::from_words(false, 38, 7, &words), parts);
            }
        }

        #[test]
        fn word_count_covers_the_significant_words() {
            let cases = [
                (0u128, 1usize),
                (1, 1),
                (u32::MAX as u128, 1),
                (u32::MAX as u128 + 1, 2),
                (u64::MAX as u128, 2),
                (u64::MAX as u128 + 1, 3),
                (u128::MAX, 4),
            ];
            for (magnitude, expected) in cases {
                let parts = DecimalParts::new(true, 38, 0, magnitude);
                assert_eq!(parts.word_count(), expected, "magnitude {magnitude}");
            }
        }

        #[test]
        fn words_past_the_fourth_are_zero() {
            let parts = DecimalParts::new(true, 38, 0, u128::MAX);
            assert_eq!(parts.word(4), 0);
            assert_eq!(parts.word(usize::MAX), 0);
        }
    }

    // Vector deserialization tests
    mod vector_tests {
        use super::*;
        use crate::datatypes::{
            sql_vector::SqlVector,
            sqldatatypes::{VectorBaseType, VectorLayoutFormat, VectorLayoutVersion},
        };

        #[test]
        fn test_vector_creation_and_validation() {
            // Test that SqlVector::try_from_f32 creates valid vectors
            let dimensions = vec![1.0, 2.0, 3.0];
            let vector = SqlVector::try_from_f32(dimensions.clone()).unwrap();

            // Only check semantic data - TDS header fields are not stored
            assert_eq!(vector.as_f32(), Some(dimensions.as_slice()));
            assert_eq!(vector.dimension_count(), 3);
        }

        #[test]
        fn test_vector_single_dimension() {
            let vector = SqlVector::try_from_f32(vec![42.5]).unwrap();
            assert_eq!(vector.as_f32(), Some(&[42.5][..]));
            assert_eq!(vector.dimension_count(), 1);
        }

        #[test]
        fn test_vector_max_dimensions() {
            let max_dim = VectorBaseType::Float32.max_dimensions();
            let dimensions: Vec<f32> = (0..max_dim).map(|i| i as f32).collect();
            let vector = SqlVector::try_from_f32(dimensions).unwrap();
            assert_eq!(vector.dimension_count(), max_dim);
        }

        #[test]
        fn test_vector_from_raw_valid() {
            let values = vec![1.0_f32, 2.0, 3.0];
            // Convert f32 values to raw bytes
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let vector = SqlVector::try_from_raw(
                VectorLayoutFormat::V1 as u8,
                VectorLayoutVersion::V1 as u8,
                VectorBaseType::Float32 as u8,
                raw_bytes,
            );

            // try_from_raw validates during construction
            assert!(vector.is_ok());
            let vector = vector.unwrap();
            assert_eq!(vector.as_f32(), Some(values.as_slice()));
        }

        #[test]
        fn test_vector_from_raw_invalid_layout_format() {
            let values = vec![1.0_f32, 2.0];
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let result = SqlVector::try_from_raw(
                0x00, // Invalid format
                VectorLayoutVersion::V1 as u8,
                VectorBaseType::Float32 as u8,
                raw_bytes,
            );

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("layout format"));
        }

        #[test]
        fn test_vector_from_raw_invalid_layout_version() {
            let values = vec![1.0_f32, 2.0];
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let result = SqlVector::try_from_raw(
                VectorLayoutFormat::V1 as u8,
                0x99, // Invalid version
                VectorBaseType::Float32 as u8,
                raw_bytes,
            );

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("layout version"));
        }

        #[test]
        fn test_vector_from_raw_invalid_base_type() {
            let values = vec![1.0_f32, 2.0];
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let result = SqlVector::try_from_raw(
                VectorLayoutFormat::V1 as u8,
                VectorLayoutVersion::V1 as u8,
                0x99, // Invalid base type
                raw_bytes,
            );

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("base type"));
        }

        #[test]
        fn test_vector_empty_dimensions() {
            let result = SqlVector::try_from_f32(vec![]);
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("at least one dimension")
            );
        }

        #[test]
        fn test_vector_too_many_dimensions() {
            let max_dim = VectorBaseType::Float32.max_dimensions();
            let dimensions: Vec<f32> = (0..(max_dim + 1)).map(|i| i as f32).collect();
            let result = SqlVector::try_from_f32(dimensions);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
        }

        #[test]
        fn test_vector_total_size() {
            let vector = SqlVector::try_from_f32(vec![1.0, 2.0, 3.0]).unwrap();
            assert_eq!(vector.total_size(), 8 + 3 * 4); // 8 byte header + 3 floats * 4 bytes
        }

        #[test]
        fn test_column_values_vector_variant() {
            let vector = SqlVector::try_from_f32(vec![1.0, 2.0, 3.0]).unwrap();
            let col_val = ColumnValues::Vector(vector);

            match col_val {
                ColumnValues::Vector(v) => {
                    assert_eq!(v.dimension_count(), 3);
                    assert_eq!(v.as_f32(), Some(&[1.0, 2.0, 3.0][..]));
                }
                _ => panic!("Expected Vector variant"),
            }
        }
    }

    mod decode_into_tests {

        use byteorder::{ByteOrder, LittleEndian};

        use crate::core::TdsResult;
        use crate::datatypes::column_values::{ColumnValues, SqlDateTime, SqlSmallDateTime};
        use crate::datatypes::decoder::{
            GenericDecoder, MAX_PLP_SIZE, PlpChunkReadLength, PlpChunkStreamReader,
            PlpColumnStream, SqlTypeDecode,
        };
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::datatypes::sqldatatypes::{
            PartialLengthType, TdsDataType, TypeInfo, TypeInfoVariant, VariableLengthTypes,
        };
        use crate::io::packet_reader::TdsPacketReader;
        use crate::query::metadata::ColumnMetadata;

        /// Byte-buffer backed mock implementing every `TdsPacketReader` method
        /// used by the decoder.
        pub(super) struct ByteReader {
            data: Vec<u8>,
            pos: usize,
        }

        impl ByteReader {
            pub(super) fn new(data: Vec<u8>) -> Self {
                Self { data, pos: 0 }
            }

            fn take(&mut self, n: usize) -> TdsResult<&[u8]> {
                if self.pos + n > self.data.len() {
                    return Err(crate::error::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "End of data",
                    )));
                }
                let slice = &self.data[self.pos..self.pos + n];
                self.pos += n;
                Ok(slice)
            }

            fn try_take<const N: usize>(&mut self) -> Option<[u8; N]> {
                let end = self.pos.checked_add(N)?;
                let bytes = self.data.get(self.pos..end)?.try_into().ok()?;
                self.pos = end;
                Some(bytes)
            }
        }

        impl TdsPacketReader for ByteReader {
            fn try_read_byte(&mut self) -> Option<u8> {
                self.try_take().map(|[value]| value)
            }
            fn try_read_int16(&mut self) -> Option<i16> {
                self.try_take().map(i16::from_le_bytes)
            }
            fn try_read_uint16(&mut self) -> Option<u16> {
                self.try_take().map(u16::from_le_bytes)
            }
            fn try_read_uint24(&mut self) -> Option<u32> {
                let [b0, b1, b2] = self.try_take()?;
                Some(u32::from_le_bytes([b0, b1, b2, 0]))
            }
            fn try_read_int32(&mut self) -> Option<i32> {
                self.try_take().map(i32::from_le_bytes)
            }
            fn try_read_uint32(&mut self) -> Option<u32> {
                self.try_take().map(u32::from_le_bytes)
            }
            fn try_read_uint40(&mut self) -> Option<u64> {
                let [b0, b1, b2, b3, b4] = self.try_take()?;
                Some(u64::from_le_bytes([b0, b1, b2, b3, b4, 0, 0, 0]))
            }
            fn try_read_int64(&mut self) -> Option<i64> {
                self.try_take().map(i64::from_le_bytes)
            }
            fn try_read_float32(&mut self) -> Option<f32> {
                self.try_take().map(f32::from_le_bytes)
            }
            fn try_read_float64(&mut self) -> Option<f64> {
                self.try_take().map(f64::from_le_bytes)
            }
            async fn read_byte(&mut self) -> TdsResult<u8> {
                Ok(self.take(1)?[0])
            }
            async fn read_int16(&mut self) -> TdsResult<i16> {
                Ok(LittleEndian::read_i16(self.take(2)?))
            }
            async fn read_uint16(&mut self) -> TdsResult<u16> {
                Ok(LittleEndian::read_u16(self.take(2)?))
            }
            async fn read_int32(&mut self) -> TdsResult<i32> {
                Ok(LittleEndian::read_i32(self.take(4)?))
            }
            async fn read_uint32(&mut self) -> TdsResult<u32> {
                Ok(LittleEndian::read_u32(self.take(4)?))
            }
            async fn read_int64(&mut self) -> TdsResult<i64> {
                Ok(LittleEndian::read_i64(self.take(8)?))
            }
            async fn read_uint64(&mut self) -> TdsResult<u64> {
                Ok(LittleEndian::read_u64(self.take(8)?))
            }
            async fn read_float32(&mut self) -> TdsResult<f32> {
                Ok(LittleEndian::read_f32(self.take(4)?))
            }
            async fn read_float64(&mut self) -> TdsResult<f64> {
                Ok(LittleEndian::read_f64(self.take(8)?))
            }
            async fn read_uint24(&mut self) -> TdsResult<u32> {
                let b = self.take(3)?;
                Ok(b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16)
            }
            async fn read_uint40(&mut self) -> TdsResult<u64> {
                let b = self.take(5)?;
                Ok(b[0] as u64
                    | (b[1] as u64) << 8
                    | (b[2] as u64) << 16
                    | (b[3] as u64) << 24
                    | (b[4] as u64) << 32)
            }
            async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
                let slice = self.take(buffer.len())?;
                buffer.copy_from_slice(slice);
                Ok(buffer.len())
            }
            async fn skip_bytes(&mut self, count: usize) -> TdsResult<()> {
                self.take(count)?;
                Ok(())
            }
            async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
                unimplemented!()
            }
            async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
                unimplemented!()
            }
            async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
                unimplemented!()
            }
            async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
                unimplemented!()
            }
            async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
                unimplemented!()
            }
            async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
                unimplemented!()
            }
            async fn read_unicode(&mut self, _len: usize) -> TdsResult<String> {
                unimplemented!()
            }
            async fn read_unicode_with_byte_length(&mut self, _len: usize) -> TdsResult<String> {
                unimplemented!()
            }
            async fn cancel_read_stream(&mut self) -> TdsResult<()> {
                unimplemented!()
            }
            fn reset_reader(&mut self) {
                self.pos = 0;
            }
        }

        fn fixed_metadata(data_type: TdsDataType, length: usize) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::FixedLen(
                        crate::datatypes::sqldatatypes::FixedLengthTypes::try_from(data_type)
                            .unwrap_or(crate::datatypes::sqldatatypes::FixedLengthTypes::Int4),
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            }
        }

        pub(super) fn varlen_metadata(data_type: TdsDataType, length: usize) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLen(
                        VariableLengthTypes::try_from(data_type)
                            .unwrap_or(VariableLengthTypes::IntN),
                        length,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            }
        }

        /// Runs both decode() and decode_into() on the same bytes and asserts
        /// that decode_into via DefaultRowWriter produces the same ColumnValues
        /// as decode().
        async fn assert_decode_equivalence(
            bytes: Vec<u8>,
            metadata: &ColumnMetadata,
        ) -> ColumnValues {
            let decoder = GenericDecoder::default();

            // Run decode()
            let mut reader1 = ByteReader::new(bytes.clone());
            let expected = decoder.decode(&mut reader1, metadata).await.unwrap();

            // Run decode_into()
            let mut reader2 = ByteReader::new(bytes);
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader2, metadata, 0, &mut writer)
                .await
                .unwrap();
            let row = writer.take_row();
            assert_eq!(row.len(), 1);
            assert_eq!(
                row[0], expected,
                "decode_into mismatch for {:?}",
                metadata.data_type
            );
            expected
        }

        #[tokio::test]
        async fn decode_into_int1() {
            let md = fixed_metadata(TdsDataType::Int1, 1);
            let val = assert_decode_equivalence(vec![42], &md).await;
            assert_eq!(val, ColumnValues::TinyInt(42));
        }

        /// A NULL `sql_variant` is a zero length and nothing else. Reading a
        /// base type and property byte anyway would consume the following
        /// column's bytes, so the sentinel after it is what proves the guard.
        ///
        /// Not reachable through a normal SQL Server response, which carries
        /// NULLs in the NBCROW null bitmap and skips the column entirely, so it
        /// has to be exercised here rather than end to end.
        #[tokio::test]
        async fn decode_into_null_sql_variant_leaves_following_bytes() {
            let md = fixed_metadata(TdsDataType::SsVariant, 0);
            let mut reader = ByteReader::new(vec![0, 0, 0, 0, 0xAB, 0xCD]);
            let decoder = GenericDecoder::default();
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await
                .unwrap();

            assert_eq!(writer.take_row()[0], ColumnValues::Null);
            assert_eq!(
                reader.read_byte().await.unwrap(),
                0xAB,
                "the NULL variant consumed the following column's bytes"
            );
        }

        /// A non-NULL variant reports its base type alongside the value, which
        /// is the whole reason the base type is threaded through the writer:
        /// an `int` and a `varchar` variant both decode to distinct
        /// `ColumnValues`, but only the wire header says what the column was
        /// declared to hold.
        #[tokio::test]
        async fn decode_into_sql_variant_reports_the_base_type() {
            let md = fixed_metadata(TdsDataType::SsVariant, 0);
            // 6-byte payload: base type INT4 (0x38), zero property bytes, then
            // the four value bytes. 0xAB trails to prove nothing overreads.
            let mut reader = ByteReader::new(vec![6, 0, 0, 0, 0x38, 0x00, 42, 0, 0, 0, 0xAB]);
            let decoder = GenericDecoder::default();
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await
                .unwrap();

            assert_eq!(writer.variant_base(0), Some(TdsDataType::Int4));
            assert_eq!(writer.take_row()[0], ColumnValues::Int(42));
            assert_eq!(reader.read_byte().await.unwrap(), 0xAB);
        }

        #[tokio::test]
        async fn decode_into_int2() {
            let md = fixed_metadata(TdsDataType::Int2, 2);
            let mut buf = [0u8; 2];
            LittleEndian::write_i16(&mut buf, -1234);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::SmallInt(-1234));
        }

        #[tokio::test]
        async fn decode_into_bigbinary_null() {
            // 0xFFFF is the USHORTLEN NULL marker (CHARBIN_NULL).
            let md = varlen_metadata(TdsDataType::BigBinary, 8);
            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(vec![0xFF, 0xFF]);
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await
                .unwrap();
            assert_eq!(writer.take_row()[0], ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_bigbinary_value() {
            let md = varlen_metadata(TdsDataType::BigBinary, 8);
            let decoder = GenericDecoder::default();
            let mut bytes = vec![4, 0]; // USHORTLEN length = 4
            bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
            let mut reader = ByteReader::new(bytes);
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await
                .unwrap();
            assert_eq!(
                writer.take_row()[0],
                ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])
            );
        }

        #[tokio::test]
        async fn decode_into_bigvarbinary_null_non_plp() {
            let md = varlen_metadata(TdsDataType::BigVarBinary, 8);
            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(vec![0xFF, 0xFF]);
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await
                .unwrap();
            assert_eq!(writer.take_row()[0], ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_bigvarbinary_value_non_plp() {
            let md = varlen_metadata(TdsDataType::BigVarBinary, 8);
            let decoder = GenericDecoder::default();
            let mut bytes = vec![3, 0]; // USHORTLEN length = 3
            bytes.extend_from_slice(&[0x01, 0x02, 0x03]);
            let mut reader = ByteReader::new(bytes);
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await
                .unwrap();
            assert_eq!(
                writer.take_row()[0],
                ColumnValues::Bytes(vec![0x01, 0x02, 0x03])
            );
        }

        #[tokio::test]
        async fn decode_into_int4() {
            let md = fixed_metadata(TdsDataType::Int4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_i32(&mut buf, 99999);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::Int(99999));
        }

        #[tokio::test]
        async fn decode_into_int8() {
            let md = fixed_metadata(TdsDataType::Int8, 8);
            let mut buf = [0u8; 8];
            LittleEndian::write_i64(&mut buf, i64::MAX);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::BigInt(i64::MAX));
        }

        #[tokio::test]
        async fn decode_into_intn_null() {
            let md = varlen_metadata(TdsDataType::IntN, 4);
            // length byte = 0 → null
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_intn_i32() {
            let md = varlen_metadata(TdsDataType::IntN, 4);
            let mut buf = vec![4u8]; // length = 4
            let mut i32_buf = [0u8; 4];
            LittleEndian::write_i32(&mut i32_buf, 777);
            buf.extend_from_slice(&i32_buf);
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Int(777));
        }

        #[tokio::test]
        async fn decode_into_flt4() {
            let md = fixed_metadata(TdsDataType::Flt4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_f32(&mut buf, 1.5);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::Real(1.5));
        }

        #[tokio::test]
        async fn decode_into_flt8() {
            let md = fixed_metadata(TdsDataType::Flt8, 8);
            let mut buf = [0u8; 8];
            LittleEndian::write_f64(&mut buf, 99.25);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::Float(99.25));
        }

        #[tokio::test]
        async fn decode_into_fltn_null() {
            let md = varlen_metadata(TdsDataType::FltN, 8);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_fltn_f32() {
            let md = varlen_metadata(TdsDataType::FltN, 4);
            let mut buf = vec![4u8];
            let mut f32_buf = [0u8; 4];
            LittleEndian::write_f32(&mut f32_buf, 2.5);
            buf.extend_from_slice(&f32_buf);
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Real(2.5));
        }

        #[tokio::test]
        async fn decode_into_bit() {
            let md = fixed_metadata(TdsDataType::Bit, 1);
            let val = assert_decode_equivalence(vec![1], &md).await;
            assert_eq!(val, ColumnValues::Bit(true));
        }

        #[tokio::test]
        async fn decode_into_bitn_null() {
            let md = varlen_metadata(TdsDataType::BitN, 1);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_bitn_true() {
            let md = varlen_metadata(TdsDataType::BitN, 1);
            let val = assert_decode_equivalence(vec![1, 1], &md).await;
            assert_eq!(val, ColumnValues::Bit(true));
        }

        #[tokio::test]
        async fn decode_into_money4() {
            let md = fixed_metadata(TdsDataType::Money4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_i32(&mut buf, 10000); // $1.00
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(val, ColumnValues::SmallMoney(_)));
        }

        #[tokio::test]
        async fn decode_into_money8() {
            let md = fixed_metadata(TdsDataType::Money, 8);
            let mut buf = [0u8; 8];
            // money is stored as msb + lsb i32 pair
            LittleEndian::write_i32(&mut buf[0..4], 0); // msb
            LittleEndian::write_i32(&mut buf[4..8], 10000); // lsb
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(val, ColumnValues::Money(_)));
        }

        #[tokio::test]
        async fn decode_into_moneyn_null() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_datetime() {
            let md = fixed_metadata(TdsDataType::DateTime, 8);
            let mut buf = [0u8; 8];
            LittleEndian::write_i32(&mut buf[0..4], 43000); // days
            LittleEndian::write_u32(&mut buf[4..8], 100); // ticks
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(
                val,
                ColumnValues::DateTime(SqlDateTime {
                    days: 43000,
                    time: 100
                })
            ));
        }

        #[tokio::test]
        async fn decode_into_smalldatetime() {
            let md = fixed_metadata(TdsDataType::DateTim4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_u16(&mut buf[0..2], 1000); // days
            LittleEndian::write_u16(&mut buf[2..4], 60); // minutes
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(
                val,
                ColumnValues::SmallDateTime(SqlSmallDateTime {
                    days: 1000,
                    time: 60
                })
            ));
        }

        #[tokio::test]
        async fn decode_into_datetimen_null() {
            let md = varlen_metadata(TdsDataType::DateTimeN, 8);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_daten() {
            let md = varlen_metadata(TdsDataType::DateN, 3);
            // length=3, then 3 bytes for date (uint24 days)
            let val = assert_decode_equivalence(vec![3, 0x01, 0x00, 0x00], &md).await;
            assert!(matches!(val, ColumnValues::Date(_)));
        }

        #[tokio::test]
        async fn decode_into_daten_null() {
            let md = varlen_metadata(TdsDataType::DateN, 3);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_guid() {
            let md = varlen_metadata(TdsDataType::Guid, 16);
            let mut buf = vec![16u8]; // length
            buf.extend_from_slice(&[1u8; 16]); // 16 bytes
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Uuid(_)));
        }

        #[tokio::test]
        async fn decode_into_guid_null() {
            let md = varlen_metadata(TdsDataType::Guid, 16);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn plp_chunk_stream_reader_supports_incremental_reads() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&0xFFFFFFFFFFFFFFFEu64.to_le_bytes());
            buf.extend_from_slice(&3u32.to_le_bytes());
            buf.extend_from_slice(b"abc");
            buf.extend_from_slice(&2u32.to_le_bytes());
            buf.extend_from_slice(b"de");
            buf.extend_from_slice(&0u32.to_le_bytes());

            let mut reader = ByteReader::new(buf);
            let mut stream = PlpChunkStreamReader::begin(&mut reader)
                .await
                .unwrap()
                .expect("not null");

            let mut out = [0u8; 2];
            let n1 = stream.read_into(&mut reader, &mut out).await.unwrap();
            assert_eq!(n1, 2);
            assert_eq!(&out, b"ab");

            let n2 = stream.read_into(&mut reader, &mut out).await.unwrap();
            assert_eq!(n2, 2);
            assert_eq!(&out, b"cd");

            let mut out_last = [0u8; 2];
            let n3 = stream.read_into(&mut reader, &mut out_last).await.unwrap();
            assert_eq!(n3, 1);
            assert_eq!(out_last[0], b'e');
            assert_eq!(stream.total_read(), 5);
            assert!(stream.reached_end());

            // cbRequest == 0 style call should consume pending terminator.
            let mut empty: [u8; 0] = [];
            let n4 = stream.read_into(&mut reader, &mut empty).await.unwrap();
            assert_eq!(n4, 0);
            assert!(stream.reached_end());
        }

        #[tokio::test]
        async fn plp_chunk_stream_reader_skip_to_end_flushes_remaining_chunks() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&5u64.to_le_bytes());
            buf.extend_from_slice(&2u32.to_le_bytes());
            buf.extend_from_slice(b"ab");
            buf.extend_from_slice(&3u32.to_le_bytes());
            buf.extend_from_slice(b"cde");
            buf.extend_from_slice(&0u32.to_le_bytes());

            let mut reader = ByteReader::new(buf);
            let mut stream = PlpChunkStreamReader::begin(&mut reader)
                .await
                .unwrap()
                .expect("not null");

            let mut one = [0u8; 1];
            let n = stream.read_into(&mut reader, &mut one).await.unwrap();
            assert_eq!(n, 1);
            assert_eq!(one[0], b'a');

            stream.skip_to_end(&mut reader).await.unwrap();
            assert!(stream.reached_end());
            assert_eq!(stream.total_read(), 5);
        }

        #[tokio::test]
        async fn plp_chunk_stream_reader_partial_read_then_skip_drains_all_bytes() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&0xFFFFFFFFFFFFFFFEu64.to_le_bytes()); // SQL_PLP_UNKNOWNLEN
            buf.extend_from_slice(&10u32.to_le_bytes()); // single chunk of 10 bytes
            buf.extend_from_slice(b"0123456789");
            buf.extend_from_slice(&0u32.to_le_bytes()); // terminator

            let mut reader = ByteReader::new(buf);
            let mut stream = PlpChunkStreamReader::begin(&mut reader)
                .await
                .unwrap()
                .expect("not null");

            let mut partial = [0u8; 3];
            let n = stream.read_into(&mut reader, &mut partial).await.unwrap();
            assert_eq!(n, 3);
            assert_eq!(&partial, b"012");
            assert!(!stream.reached_end());

            stream.skip_to_end(&mut reader).await.unwrap();
            assert!(stream.reached_end());
            assert_eq!(stream.total_read(), 10);
        }

        #[tokio::test]
        async fn plp_column_stream_repeated_small_reads_exhaust_payload() {
            let payload = b"hello world";
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let mut reader = ByteReader::new(plp_wire(payload));
            let mut stream = PlpColumnStream::begin(&md, &mut reader)
                .await
                .unwrap()
                .unwrap();

            let mut collected = Vec::new();
            let mut chunk = [0u8; 4];
            loop {
                let n = stream.read_into(&mut reader, &mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                collected.extend_from_slice(&chunk[..n]);
            }
            assert_eq!(collected, payload);
            assert!(stream.reached_end());
        }

        // -------------------------------------------------------------------
        // PlpColumnStream tests — type-aware wrapper over PlpChunkStreamReader
        // -------------------------------------------------------------------

        pub(super) fn plp_metadata(
            data_type: TdsDataType,
            partial_type: PartialLengthType,
            collation: Option<crate::token::tokens::SqlCollation>,
        ) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length: 0xFFFF,
                    type_info_variant: TypeInfoVariant::PartialLen(
                        partial_type,
                        Some(0xFFFF),
                        collation,
                        None,
                        None,
                    ),
                },
                column_name: "col".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            }
        }

        fn plp_wire(payload: &[u8]) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.extend_from_slice(&0xFFFFFFFFFFFFFFFEu64.to_le_bytes()); // SQL_PLP_UNKNOWNLEN
            buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(payload);
            buf.extend_from_slice(&0u32.to_le_bytes()); // terminator
            buf
        }

        #[tokio::test]
        async fn plp_column_stream_null_returns_none() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&0xFFFFFFFFFFFFFFFFu64.to_le_bytes()); // SQL_PLP_NULL
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let mut reader = ByteReader::new(buf);
            let result = PlpColumnStream::begin(&md, &mut reader).await.unwrap();
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn plp_column_stream_binary_kind() {
            let payload = b"binarydata";
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let mut reader = ByteReader::new(plp_wire(payload));
            let mut stream = PlpColumnStream::begin(&md, &mut reader)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stream.plp_type(), PartialLengthType::BigVarBinary);
            let mut out = vec![0u8; payload.len()];
            let n = stream.read_into(&mut reader, &mut out).await.unwrap();
            assert_eq!(n, payload.len());
            assert_eq!(&out, payload);
            assert!(stream.reached_end());
        }

        #[tokio::test]
        async fn plp_column_stream_unicode_text_kind() {
            let payload = b"hi"; // raw bytes, not actually UTF-16 but fine for stream test
            let md = plp_metadata(TdsDataType::NVarChar, PartialLengthType::NVarChar, None);
            let mut reader = ByteReader::new(plp_wire(payload));
            let mut stream = PlpColumnStream::begin(&md, &mut reader)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stream.plp_type(), PartialLengthType::NVarChar);
            let mut out = vec![0u8; 2];
            let n = stream.read_into(&mut reader, &mut out).await.unwrap();
            assert_eq!(n, 2);
            assert!(stream.reached_end());
        }

        #[tokio::test]
        async fn plp_column_stream_bigvarchar_kind_carries_collation() {
            let payload = b"hello";
            let col = crate::token::tokens::SqlCollation {
                info: 0x0409_0034,
                lcid_language_id: 0x0409,
                col_flags: 0,
                sort_id: 52,
            };
            let md = plp_metadata(
                TdsDataType::BigVarChar,
                PartialLengthType::BigVarChar,
                Some(col),
            );
            let mut reader = ByteReader::new(plp_wire(payload));
            let mut stream = PlpColumnStream::begin(&md, &mut reader)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stream.plp_type(), PartialLengthType::BigVarChar);
            assert!(stream.collation().is_some());
            let mut out = vec![0u8; 5];
            let n = stream.read_into(&mut reader, &mut out).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&out, b"hello");
            assert!(stream.reached_end());
        }

        #[tokio::test]
        async fn plp_column_stream_xml_kind() {
            let payload = b"<r/>";
            let md = plp_metadata(TdsDataType::Xml, PartialLengthType::Xml, None);
            let mut reader = ByteReader::new(plp_wire(payload));
            let mut stream = PlpColumnStream::begin(&md, &mut reader)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stream.plp_type(), PartialLengthType::Xml);
            stream.skip_to_end(&mut reader).await.unwrap();
            assert!(stream.reached_end());
            assert_eq!(stream.total_read(), 4);
        }

        #[tokio::test]
        async fn plp_column_stream_json_kind() {
            let payload = b"{}";
            let md = plp_metadata(TdsDataType::Json, PartialLengthType::Json, None);
            let mut reader = ByteReader::new(plp_wire(payload));
            let mut stream = PlpColumnStream::begin(&md, &mut reader)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stream.plp_type(), PartialLengthType::Json);
            let mut out = vec![0u8; 2];
            let n = stream.read_into(&mut reader, &mut out).await.unwrap();
            assert_eq!(n, 2);
            assert!(stream.reached_end());
        }

        #[tokio::test]
        async fn plp_column_stream_udt_kind() {
            let payload = b"\x01\x02\x03";
            let md = plp_metadata(TdsDataType::Udt, PartialLengthType::Udt, None);
            let mut reader = ByteReader::new(plp_wire(payload));
            let mut stream = PlpColumnStream::begin(&md, &mut reader)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stream.plp_type(), PartialLengthType::Udt);
            let mut out = vec![0u8; 3];
            let n = stream.read_into(&mut reader, &mut out).await.unwrap();
            assert_eq!(n, 3);
            assert!(stream.reached_end());
        }

        #[tokio::test]
        async fn plp_column_stream_rejects_non_plp_metadata() {
            let md = varlen_metadata(TdsDataType::NVarChar, 100); // non-PLP VarLen
            let buf = plp_wire(b"x"); // header bytes, won't matter
            let mut reader = ByteReader::new(buf);
            let err = PlpColumnStream::begin(&md, &mut reader).await.unwrap_err();
            assert!(
                err.to_string().contains("is not a PLP type"),
                "unexpected: {err}"
            );
        }

        #[tokio::test]
        async fn decode_xml_rejects_non_plp_metadata() {
            let md = varlen_metadata(TdsDataType::Xml, 100);
            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(plp_wire(b"x"));
            let err = decoder.decode(&mut reader, &md).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("XML column metadata is not partially-length-prefixed"),
                "unexpected: {err}"
            );
        }

        #[tokio::test]
        async fn decode_json_rejects_non_plp_metadata() {
            let md = varlen_metadata(TdsDataType::Json, 100);
            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(plp_wire(b"x"));
            let err = decoder.decode(&mut reader, &md).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("JSON column metadata is not partially-length-prefixed"),
                "unexpected: {err}"
            );
        }

        #[tokio::test]
        async fn decode_udt_rejects_non_plp_metadata() {
            let md = varlen_metadata(TdsDataType::Udt, 100);
            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(plp_wire(b"x"));
            let err = decoder.decode(&mut reader, &md).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("UDT column metadata is not partially-length-prefixed"),
                "unexpected: {err}"
            );
        }

        #[tokio::test]
        async fn plp_chunk_stream_reader_known_length_overflow_errors() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&4u64.to_le_bytes());
            buf.extend_from_slice(&5u32.to_le_bytes());

            let mut reader = ByteReader::new(buf);
            let mut stream = PlpChunkStreamReader::begin(&mut reader)
                .await
                .unwrap()
                .expect("not null");

            let mut out = [0u8; 1];
            let err = stream.read_into(&mut reader, &mut out).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("PLP chunk exceeds declared length"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn plp_chunk_stream_reader_known_length_early_terminator_errors() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&4u64.to_le_bytes());
            buf.extend_from_slice(&2u32.to_le_bytes());
            buf.extend_from_slice(b"ab");
            buf.extend_from_slice(&0u32.to_le_bytes());

            let mut reader = ByteReader::new(buf);
            let mut stream = PlpChunkStreamReader::begin(&mut reader)
                .await
                .unwrap()
                .expect("not null");

            let mut out = [0u8; 4];
            let err = stream.read_into(&mut reader, &mut out).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("PLP stream ended before declared length was reached"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn plp_chunk_stream_reader_chunk_count_limit_errors() {
            let mut stream = PlpChunkStreamReader {
                length: PlpChunkReadLength::Unknown,
                chunk_remaining: 0,
                reached_end: false,
                total_read: 0,
                chunks_seen: GenericDecoder::MAX_PLP_CHUNKS,
            };
            let mut reader = ByteReader::new(vec![1, 0, 0, 0]);
            let mut out = [0u8; 1];

            let err = stream.read_into(&mut reader, &mut out).await.unwrap_err();
            assert!(
                err.to_string().contains("Too many PLP chunks"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn plp_chunk_stream_reader_accumulated_size_limit_errors() {
            let mut stream = PlpChunkStreamReader {
                length: PlpChunkReadLength::Known((MAX_PLP_SIZE as u64) + 1),
                chunk_remaining: 0,
                reached_end: false,
                total_read: MAX_PLP_SIZE,
                chunks_seen: 0,
            };
            let mut reader = ByteReader::new(vec![1, 0, 0, 0]);
            let mut out = [0u8; 1];

            let err = stream.read_into(&mut reader, &mut out).await.unwrap_err();
            assert!(
                err.to_string().contains("PLP accumulated size"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn read_plp_bytes_rejects_oversized_chunk_in_existing_path() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&0xFFFFFFFFFFFFFFFEu64.to_le_bytes());
            let oversized = (GenericDecoder::MAX_PLP_CHUNK_SIZE + 1) as u32;
            buf.extend_from_slice(&oversized.to_le_bytes());

            let mut reader = ByteReader::new(buf);
            let err = GenericDecoder::read_plp_bytes(&mut reader)
                .await
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("exceeds maximum allowed chunk size"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn decode_into_bigbinary() {
            let md = varlen_metadata(TdsDataType::BigBinary, 4);
            let mut buf = Vec::new();
            // u16 length = 4
            buf.extend_from_slice(&[4, 0]);
            buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        }

        #[tokio::test]
        async fn decode_into_nvarchar() {
            // non-PLP, non-LOB: u16 length + bytes
            let md = varlen_metadata(TdsDataType::NVarChar, 100);
            let text_bytes = b"hi";
            let mut buf = Vec::new();
            // u16 length = 2
            buf.extend_from_slice(&[2, 0]);
            buf.extend_from_slice(text_bytes);
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::String(_)));
        }

        #[tokio::test]
        async fn decode_into_nvarchar_null() {
            let md = varlen_metadata(TdsDataType::NVarChar, 100);
            // 0xFFFF = NULL
            let val = assert_decode_equivalence(vec![0xFF, 0xFF], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_decimaln_null() {
            let md = ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::DecimalN,
                type_info: TypeInfo {
                    tds_type: TdsDataType::DecimalN,
                    length: 9,
                    type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                        VariableLengthTypes::DecimalN,
                        9,
                        18,
                        5,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            };
            // length byte = 0 → NULL
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_decimaln_value() {
            let md = ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::DecimalN,
                type_info: TypeInfo {
                    tds_type: TdsDataType::DecimalN,
                    length: 9,
                    type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                        VariableLengthTypes::DecimalN,
                        9,
                        18,
                        2,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            };
            // length=5, sign=1 (positive), one i32 part = 12345
            let mut buf = vec![5u8, 1u8];
            let mut part = [0u8; 4];
            LittleEndian::write_i32(&mut part, 12345);
            buf.extend_from_slice(&part);
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Decimal(_)));
        }

        // ── helpers for new tests ──────────────────────────────────────

        fn precision_scale_metadata(
            data_type: TdsDataType,
            length: usize,
            precision: u8,
            scale: u8,
        ) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                        VariableLengthTypes::try_from(data_type)
                            .unwrap_or(VariableLengthTypes::DecimalN),
                        length,
                        precision,
                        scale,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            }
        }

        fn scale_metadata(data_type: TdsDataType, length: usize, scale: u8) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenScale(
                        VariableLengthTypes::try_from(data_type)
                            .unwrap_or(VariableLengthTypes::TimeN),
                        scale,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            }
        }

        fn ssvariant_metadata() -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::SsVariant,
                type_info: TypeInfo {
                    tds_type: TdsDataType::SsVariant,
                    length: 8009,
                    type_info_variant: TypeInfoVariant::VarLen(
                        VariableLengthTypes::SsVariant,
                        8009,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            }
        }

        async fn assert_decode_err(bytes: Vec<u8>, metadata: &ColumnMetadata) {
            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(bytes);
            assert!(decoder.decode(&mut reader, metadata).await.is_err());
        }

        // ── SQL_VARIANT tests ──────────────────────────────────────────

        #[tokio::test]
        async fn ssvariant_length_underflow() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            // length=1, so checked_sub(2) on remaining after base_type+prop fails
            LittleEndian::write_u32(&mut [0u8; 4], 1);
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.push(TdsDataType::Int4 as u8); // base_type
            buf.push(0); // prop_bytes
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_unexpected_prop_bytes() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            buf.extend_from_slice(&10u32.to_le_bytes()); // length=10
            buf.push(TdsDataType::Int4 as u8); // base_type
            buf.push(5); // prop_bytes=5 (invalid)
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_zero_prop_unexpected_type() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            // IntN is variable-length, not fixed-length, so FixedLengthTypes::try_from fails.
            // Then it falls through to the match on Guid/DateN, and IntN doesn't match → error.
            buf.extend_from_slice(&6u32.to_le_bytes()); // length=6 (2 header + 0 props + 4 data)
            buf.push(TdsDataType::IntN as u8); // base_type
            buf.push(0); // prop_bytes=0
            // provide 4 bytes of data that will never be read
            buf.extend_from_slice(&[0; 4]);
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_one_prop_unexpected_type() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            // Guid has 0 prop bytes, not 1 → error in decode_one_byte_variant
            buf.extend_from_slice(&20u32.to_le_bytes()); // length
            buf.push(TdsDataType::Guid as u8); // base_type
            buf.push(1); // prop_bytes=1
            buf.push(0); // scale byte (prop data)
            buf.extend_from_slice(&[0; 16]); // data
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_two_prop_unexpected_type() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            buf.extend_from_slice(&6u32.to_le_bytes()); // length
            buf.push(TdsDataType::IntN as u8); // not binary/decimal → error
            buf.push(2); // prop_bytes=2
            buf.extend_from_slice(&[0; 4]); // prop data + value data
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_seven_prop_unexpected_type() {
            // Regression for fuzz crash-80c55599: a SQL_VARIANT advertising 7
            // property bytes with a non-character base type used to trip a
            // debug assertion in decode_seven_propbyte_variant. It must now
            // return a ProtocolError instead of panicking.
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            buf.extend_from_slice(&20u32.to_le_bytes()); // length
            buf.push(TdsDataType::IntN as u8); // not a character type → error
            buf.push(7); // prop_bytes=7 selects the character-type decoder
            buf.extend_from_slice(&[0; 11]); // padding, never read
            assert_decode_err(buf, &md).await;
        }

        // ── Decimal error paths ────────────────────────────────────────

        #[tokio::test]
        async fn decimal_wrong_type_info_variant() {
            // Use VarLen instead of VarLenPrecisionScale
            let md = varlen_metadata(TdsDataType::DecimalN, 9);
            // length byte = 5, sign = 1, one i32 part
            let mut buf = vec![5u8, 1u8];
            buf.extend_from_slice(&42i32.to_le_bytes());
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn decimal_large_valid() {
            let md = precision_scale_metadata(TdsDataType::DecimalN, 17, 38, 0);
            // length=17 → 16 magnitude bytes = 4 int parts, the widest valid decimal
            let mut buf = vec![17u8, 1u8]; // length=17, sign=positive
            for _ in 0..4 {
                buf.extend_from_slice(&1i32.to_le_bytes());
            }
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Decimal(_)));
        }

        #[tokio::test]
        async fn decimal_too_many_int_parts_rejected() {
            let md = precision_scale_metadata(TdsDataType::DecimalN, 21, 38, 0);
            // length=21 → 20 magnitude bytes = 5 int parts, which no valid decimal produces
            let mut buf = vec![21u8, 1u8];
            for _ in 0..5 {
                buf.extend_from_slice(&1i32.to_le_bytes());
            }
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn decimal_oversized_partial_word_length_rejected() {
            let md = precision_scale_metadata(TdsDataType::DecimalN, 18, 38, 0);
            // length=18 → 17 magnitude bytes, one byte past the 128-bit limit.
            // Rounding down would have accepted this as 4 parts and left the
            // trailing byte unread.
            let mut buf = vec![18u8, 1u8];
            buf.extend_from_slice(&[1u8; 17]);
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn decimal_partial_trailing_word_is_fully_consumed() {
            let md = precision_scale_metadata(TdsDataType::DecimalN, 9, 18, 2);
            // length=7 → 6 magnitude bytes: a full word plus a 2-byte partial
            // word that must be zero-padded and consumed, leaving the reader
            // positioned on the following field rather than mid-value.
            let mut buf = vec![7u8, 1u8];
            buf.extend_from_slice(&12345i32.to_le_bytes());
            buf.extend_from_slice(&[0u8, 0u8]);
            let trailing = 0x5A5A_5A5Ai32;
            buf.extend_from_slice(&trailing.to_le_bytes());

            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(buf);
            let val = decoder.decode(&mut reader, &md).await.unwrap();
            match val {
                ColumnValues::Decimal(parts) => assert_eq!(parts.to_string(), "123.45"),
                other => panic!("expected Decimal, got {other:?}"),
            }
            assert_eq!(reader.read_int32().await.unwrap(), trailing);
        }

        #[tokio::test]
        async fn decimal_wire_widths_reassemble_the_magnitude() {
            // The four lengths a real server emits: 1 sign byte plus 4, 8, 12 or
            // 16 magnitude bytes.
            let cases: [(u8, u128); 4] = [
                (5, 0xDEAD_BEEF),
                (9, 0x0123_4567_89AB_CDEF),
                (13, 0x0000_0009_8765_4321_0FED_CBA9),
                (17, u128::MAX >> 1),
            ];

            for (length, magnitude) in cases {
                let md = precision_scale_metadata(TdsDataType::DecimalN, length.into(), 38, 0);
                let mut buf = vec![length, 1u8];
                let bytes = magnitude.to_le_bytes();
                buf.extend_from_slice(&bytes[..(length - 1) as usize]);

                match assert_decode_equivalence(buf, &md).await {
                    ColumnValues::Decimal(parts) => {
                        assert_eq!(parts.magnitude(), magnitude, "wire length {length}");
                        assert!(parts.is_positive);
                    }
                    other => panic!("expected Decimal, got {other:?}"),
                }
            }
        }

        #[tokio::test]
        async fn decimal_negative_sign_byte_is_honored() {
            let md = precision_scale_metadata(TdsDataType::DecimalN, 5, 18, 2);
            let mut buf = vec![5u8, 0u8];
            buf.extend_from_slice(&12345i32.to_le_bytes());

            match assert_decode_equivalence(buf, &md).await {
                ColumnValues::Decimal(parts) => assert_eq!(parts.to_string(), "-123.45"),
                other => panic!("expected Decimal, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn time_scale_0() {
            let md = scale_metadata(TdsDataType::TimeN, 3, 0);
            // length=3 → read_uint24 → value=1 (1 second in scale-0 units)
            // time_nanoseconds = 1 * 10_000_000
            let mut buf = vec![3u8]; // length byte
            buf.extend_from_slice(&[1, 0, 0]); // uint24 = 1
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 10_000_000);
                    assert_eq!(t.scale, 0);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_1() {
            let md = scale_metadata(TdsDataType::TimeN, 3, 1);
            let mut buf = vec![3u8];
            buf.extend_from_slice(&[1, 0, 0]);
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 1_000_000);
                    assert_eq!(t.scale, 1);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_2() {
            let md = scale_metadata(TdsDataType::TimeN, 3, 2);
            let mut buf = vec![3u8];
            buf.extend_from_slice(&[1, 0, 0]);
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 100_000);
                    assert_eq!(t.scale, 2);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_4() {
            let md = scale_metadata(TdsDataType::TimeN, 4, 4);
            let mut buf = vec![4u8];
            buf.extend_from_slice(&1u32.to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 1_000);
                    assert_eq!(t.scale, 4);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_5() {
            let md = scale_metadata(TdsDataType::TimeN, 5, 5);
            let mut buf = vec![5u8];
            buf.extend_from_slice(&[1, 0, 0, 0, 0]); // uint40 = 1
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 100);
                    assert_eq!(t.scale, 5);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_6() {
            let md = scale_metadata(TdsDataType::TimeN, 5, 6);
            let mut buf = vec![5u8];
            buf.extend_from_slice(&[1, 0, 0, 0, 0]);
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 10);
                    assert_eq!(t.scale, 6);
                }
                _ => panic!("expected Time"),
            }
        }

        // ── DateTime2 / DateTimeOffset error paths ─────────────────────

        #[tokio::test]
        async fn datetime2_byte_len_underflow() {
            let md = scale_metadata(TdsDataType::DateTime2N, 2, 7);
            // length=2 → byte_len < 3 → checked_sub error
            let buf = vec![2u8, 0, 0];
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn datetimeoffset_byte_len_underflow() {
            let md = scale_metadata(TdsDataType::DateTimeOffsetN, 1, 7);
            // length=1 → byte_len < 2 → checked_sub error in read_datetime_offset
            let buf = vec![1u8, 0];
            assert_decode_err(buf, &md).await;
        }

        // ── IntN unusual lengths ───────────────────────────────────────

        #[tokio::test]
        async fn intn_tinyint() {
            let md = varlen_metadata(TdsDataType::IntN, 1);
            let val = assert_decode_equivalence(vec![1u8, 42u8], &md).await;
            assert_eq!(val, ColumnValues::TinyInt(42));
        }

        #[tokio::test]
        async fn intn_smallint() {
            let md = varlen_metadata(TdsDataType::IntN, 2);
            let mut buf = vec![2u8];
            buf.extend_from_slice(&(-100i16).to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::SmallInt(-100));
        }

        #[tokio::test]
        async fn intn_invalid_length() {
            let md = varlen_metadata(TdsDataType::IntN, 4);
            let buf = vec![3u8, 0, 0, 0]; // byte_len=3 → invalid
            assert_decode_err(buf, &md).await;
        }

        // ── GUID error paths ──────────────────────────────────────────

        #[tokio::test]
        async fn guid_invalid_length() {
            let md = varlen_metadata(TdsDataType::Guid, 16);
            // length=5 (not 0 or 16) → error
            let mut buf = vec![5u8];
            buf.extend_from_slice(&[0; 5]);
            assert_decode_err(buf, &md).await;
        }

        // ── FltN paths ────────────────────────────────────────────────

        #[tokio::test]
        async fn fltn_f64() {
            let md = varlen_metadata(TdsDataType::FltN, 8);
            let mut buf = vec![8u8];
            buf.extend_from_slice(&99.5f64.to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Float(99.5));
        }

        // ── MoneyN paths ──────────────────────────────────────────────

        #[tokio::test]
        async fn moneyn_smallmoney() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let mut buf = vec![4u8];
            buf.extend_from_slice(&10000i32.to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::SmallMoney(_)));
        }

        #[tokio::test]
        async fn moneyn_money() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let mut buf = vec![8u8];
            buf.extend_from_slice(&0i32.to_le_bytes()); // msb
            buf.extend_from_slice(&10000i32.to_le_bytes()); // lsb
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Money(_)));
        }

        #[tokio::test]
        async fn moneyn_invalid_length() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let buf = vec![3u8, 0, 0, 0];
            assert_decode_err(buf, &md).await;
        }

        // ── DateTimeN paths ───────────────────────────────────────────

        #[tokio::test]
        async fn datetimen_smalldatetime() {
            let md = varlen_metadata(TdsDataType::DateTimeN, 8);
            let mut buf = vec![4u8];
            buf.extend_from_slice(&1000u16.to_le_bytes()); // days
            buf.extend_from_slice(&60u16.to_le_bytes()); // minutes
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(
                val,
                ColumnValues::SmallDateTime(SqlSmallDateTime {
                    days: 1000,
                    time: 60
                })
            ));
        }

        #[tokio::test]
        async fn datetimen_datetime() {
            let md = varlen_metadata(TdsDataType::DateTimeN, 8);
            let mut buf = vec![8u8];
            buf.extend_from_slice(&43000i32.to_le_bytes()); // days
            buf.extend_from_slice(&100u32.to_le_bytes()); // ticks
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(
                val,
                ColumnValues::DateTime(SqlDateTime {
                    days: 43000,
                    time: 100
                })
            ));
        }

        // ── BigBinary empty ────────────────────────────────────────────

        #[tokio::test]
        async fn bigbinary_empty() {
            let md = varlen_metadata(TdsDataType::BigBinary, 100);
            let val = assert_decode_equivalence(vec![0x00, 0x00], &md).await;
            assert_eq!(val, ColumnValues::Bytes(vec![]));
        }

        // ── DateTime2N valid decode ───────────────────────────────────

        #[tokio::test]
        async fn datetime2n_value() {
            let md = scale_metadata(TdsDataType::DateTime2N, 8, 7);
            // length=8 → time_byte_len=5, date=3 bytes
            let mut buf = vec![8u8];
            buf.extend_from_slice(&[0, 0, 0, 0, 0]); // time (uint40 = 0)
            buf.extend_from_slice(&[1, 0, 0]); // date (uint24 = 1)
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::DateTime2(_)));
        }

        // ── DateTimeOffsetN valid decode ──────────────────────────────

        #[tokio::test]
        async fn datetimeoffsetn_value() {
            let md = scale_metadata(TdsDataType::DateTimeOffsetN, 10, 7);
            // length=10 → datetime2_byte_len=8, offset=2 bytes
            let mut buf = vec![10u8];
            buf.extend_from_slice(&[0, 0, 0, 0, 0]); // time (uint40)
            buf.extend_from_slice(&[1, 0, 0]); // date (uint24)
            buf.extend_from_slice(&60i16.to_le_bytes()); // offset minutes
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::DateTimeOffset(_)));
        }
    }

    /// Tests for the `RowWriter` sink API: values whose storage is supplied by
    /// the writer and filled straight from the wire.
    mod sink_destination_tests {
        use std::mem::MaybeUninit;

        use super::decode_into_tests::{ByteReader, plp_metadata, varlen_metadata};
        use crate::datatypes::column_values::{
            SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
            SqlSmallMoney, SqlTime, SqlXml,
        };
        use crate::datatypes::decoder::{DecimalParts, GenericDecoder};
        use crate::datatypes::row_writer::{RowWriter, ValueKind};
        use crate::datatypes::sql_json::SqlJson;
        use crate::datatypes::sql_string::{EncodingType, SqlString};
        use crate::datatypes::sql_vector::SqlVector;
        use crate::datatypes::sqldatatypes::{PartialLengthType, TdsDataType};
        use crate::query::metadata::ColumnMetadata;
        use crate::token::tokens::SqlCollation;
        use uuid::Uuid;

        /// What a [`RecordingSink`] observed for one column.
        #[derive(Debug, PartialEq)]
        enum Event {
            Null,
            /// Payload delivered through the sink, with the kind that was requested.
            Sunk {
                bytes: Vec<u8>,
                encoding: Option<EncodingType>,
                complete: bool,
            },
            /// Payload delivered through the owned path instead.
            OwnedBytes(Vec<u8>),
            /// Payload delivered through the owned path instead.
            OwnedString(Vec<u8>),
        }

        /// Row writer that accepts sink destinations and records what it received.
        ///
        /// `accept` gates whether destinations are handed out at all, so the same
        /// writer exercises both the sink path and the decline-to-owned path.
        struct RecordingSink {
            accept: bool,
            destination_len: Option<usize>,
            destination_requests: usize,
            events: Vec<Event>,
            pending: Option<(usize, Option<EncodingType>)>,
            storage: Vec<u8>,
        }

        impl RecordingSink {
            fn new(accept: bool) -> Self {
                Self {
                    accept,
                    destination_len: None,
                    destination_requests: 0,
                    events: Vec::new(),
                    pending: None,
                    storage: Vec::new(),
                }
            }

            fn with_destination_len(length: usize) -> Self {
                Self {
                    destination_len: Some(length),
                    ..Self::new(true)
                }
            }
        }

        impl RowWriter for RecordingSink {
            fn value_destination<'a>(
                &'a mut self,
                _col: usize,
                kind: ValueKind<'_>,
                length: usize,
            ) -> Option<&'a mut [MaybeUninit<u8>]> {
                self.destination_requests += 1;
                if !self.accept {
                    return None;
                }
                let length = self.destination_len.unwrap_or(length);
                let start = self.storage.len();
                // 0xAA marks untouched bytes so a short fill is visible in asserts.
                self.storage.resize(start + length, 0xAA);
                let encoding = match kind {
                    ValueKind::Bytes => None,
                    ValueKind::String(encoding) => Some(encoding.clone()),
                };
                self.pending = Some((start, encoding));
                let storage = &mut self.storage[start..];
                // SAFETY: initialized bytes are valid `MaybeUninit<u8>` values.
                Some(unsafe {
                    std::slice::from_raw_parts_mut(
                        storage.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                        storage.len(),
                    )
                })
            }

            fn commit_value(&mut self, _col: usize, complete: bool) {
                let (start, encoding) = self.pending.take().expect("commit without destination");
                self.events.push(Event::Sunk {
                    bytes: self.storage[start..].to_vec(),
                    encoding,
                    complete,
                });
                self.storage.truncate(start);
            }

            fn write_null(&mut self, _col: usize) {
                self.events.push(Event::Null);
            }
            fn write_bytes(&mut self, _col: usize, val: Vec<u8>) {
                self.events.push(Event::OwnedBytes(val));
            }
            fn write_string(&mut self, _col: usize, val: SqlString) {
                self.events.push(Event::OwnedString(val.bytes));
            }

            fn write_bool(&mut self, _col: usize, _val: bool) {}
            fn write_u8(&mut self, _col: usize, _val: u8) {}
            fn write_i16(&mut self, _col: usize, _val: i16) {}
            fn write_i32(&mut self, _col: usize, _val: i32) {}
            fn write_i64(&mut self, _col: usize, _val: i64) {}
            fn write_f32(&mut self, _col: usize, _val: f32) {}
            fn write_f64(&mut self, _col: usize, _val: f64) {}
            fn write_decimal(&mut self, _col: usize, _val: DecimalParts) {}
            fn write_numeric(&mut self, _col: usize, _val: DecimalParts) {}
            fn write_date(&mut self, _col: usize, _val: SqlDate) {}
            fn write_time(&mut self, _col: usize, _val: SqlTime) {}
            fn write_datetime(&mut self, _col: usize, _val: SqlDateTime) {}
            fn write_smalldatetime(&mut self, _col: usize, _val: SqlSmallDateTime) {}
            fn write_datetime2(&mut self, _col: usize, _val: SqlDateTime2) {}
            fn write_datetimeoffset(&mut self, _col: usize, _val: SqlDateTimeOffset) {}
            fn write_money(&mut self, _col: usize, _val: SqlMoney) {}
            fn write_smallmoney(&mut self, _col: usize, _val: SqlSmallMoney) {}
            fn write_uuid(&mut self, _col: usize, _val: Uuid) {}
            fn write_xml(&mut self, _col: usize, _val: SqlXml) {}
            fn write_json(&mut self, _col: usize, _val: SqlJson) {}
            fn write_vector(&mut self, _col: usize, _val: SqlVector) {}
            fn end_row(&mut self) {}
        }

        fn sunk(bytes: &[u8], encoding: Option<EncodingType>) -> Event {
            Event::Sunk {
                bytes: bytes.to_vec(),
                encoding,
                complete: true,
            }
        }

        /// Frames a known-length PLP value, splitting the payload into chunks of
        /// at most `chunk` bytes.
        fn plp_known(payload: &[u8], chunk: usize) -> Vec<u8> {
            let mut buf = (payload.len() as u64).to_le_bytes().to_vec();
            for part in payload.chunks(chunk.max(1)) {
                buf.extend_from_slice(&(part.len() as u32).to_le_bytes());
                buf.extend_from_slice(part);
            }
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf
        }

        fn plp_unknown(payload: &[u8]) -> Vec<u8> {
            let mut buf = 0xFFFFFFFFFFFFFFFEu64.to_le_bytes().to_vec();
            buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(payload);
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf
        }

        fn plp_null() -> Vec<u8> {
            0xFFFFFFFFFFFFFFFFu64.to_le_bytes().to_vec()
        }

        async fn decode_generic(wire: Vec<u8>, md: &ColumnMetadata, accept: bool) -> Vec<Event> {
            let mut reader = ByteReader::new(wire);
            let mut writer = RecordingSink::new(accept);
            GenericDecoder::default()
                .decode_into(&mut reader, md, 0, &mut writer)
                .await
                .unwrap();
            writer.events
        }

        /// Character columns route through `GenericDecoder` too, so both kinds
        /// exercise the same entry point.
        async fn decode_string(wire: Vec<u8>, md: &ColumnMetadata, accept: bool) -> Vec<Event> {
            decode_generic(wire, md, accept).await
        }

        // ---------------------------------------------------------------
        // Binary
        // ---------------------------------------------------------------

        #[tokio::test]
        async fn bigvarbinary_non_plp_never_requests_a_destination() {
            let md = varlen_metadata(TdsDataType::BigVarBinary, 8000);
            let mut wire = 4u16.to_le_bytes().to_vec();
            wire.extend_from_slice(&[1, 2, 3, 4]);
            assert_eq!(
                decode_generic(wire, &md, true).await,
                vec![Event::OwnedBytes(vec![1, 2, 3, 4])]
            );
        }

        #[tokio::test]
        async fn bigbinary_never_requests_a_destination() {
            let md = varlen_metadata(TdsDataType::BigBinary, 8000);
            let mut wire = 3u16.to_le_bytes().to_vec();
            wire.extend_from_slice(&[9, 8, 7]);
            assert_eq!(
                decode_generic(wire, &md, true).await,
                vec![Event::OwnedBytes(vec![9, 8, 7])]
            );
        }

        #[tokio::test]
        async fn bigvarbinary_plp_known_length_uses_sink_across_chunks() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let payload: Vec<u8> = (0..=255u8).collect();
            assert_eq!(
                decode_generic(plp_known(&payload, 30), &md, true).await,
                vec![sunk(&payload, None)]
            );
        }

        #[tokio::test]
        async fn declining_writer_falls_back_to_owned_path() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let payload: Vec<u8> = (0..64u8).collect();
            assert_eq!(
                decode_generic(plp_known(&payload, 7), &md, false).await,
                vec![Event::OwnedBytes(payload)]
            );
        }

        #[tokio::test]
        async fn plp_unknown_length_never_requests_a_destination() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let payload: Vec<u8> = (0..40u8).collect();
            assert_eq!(
                decode_generic(plp_unknown(&payload), &md, true).await,
                vec![Event::OwnedBytes(payload)]
            );
        }

        #[tokio::test]
        async fn plp_null_never_requests_a_destination() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            assert_eq!(
                decode_generic(plp_null(), &md, true).await,
                vec![Event::Null]
            );
        }

        #[tokio::test]
        async fn non_plp_null_marker_never_requests_a_destination() {
            let md = varlen_metadata(TdsDataType::BigVarBinary, 8000);
            let wire = 0xFFFFu16.to_le_bytes().to_vec();
            assert_eq!(decode_generic(wire, &md, true).await, vec![Event::Null]);
        }

        #[tokio::test]
        async fn empty_plp_value_sinks_an_empty_slice() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            assert_eq!(
                decode_generic(plp_known(&[], 1), &md, true).await,
                vec![sunk(&[], None)]
            );
        }

        // ---------------------------------------------------------------
        // Strings — raw wire bytes are handed over without transcoding
        // ---------------------------------------------------------------

        #[tokio::test]
        async fn nvarchar_non_plp_never_requests_a_destination() {
            let md = varlen_metadata(TdsDataType::NVarChar, 0xFF);
            let utf16: Vec<u8> = "hé".encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut wire = (utf16.len() as u16).to_le_bytes().to_vec();
            wire.extend_from_slice(&utf16);
            assert_eq!(
                decode_string(wire, &md, true).await,
                vec![Event::OwnedString(utf16)]
            );
        }

        #[tokio::test]
        async fn nvarchar_plp_known_length_sinks_raw_wire_bytes_across_chunks() {
            let md = plp_metadata(TdsDataType::NVarChar, PartialLengthType::NVarChar, None);
            let utf16: Vec<u8> = "the quick brown fox"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            assert_eq!(
                decode_string(plp_known(&utf16, 6), &md, true).await,
                vec![sunk(&utf16, Some(EncodingType::Utf16))]
            );
        }

        #[tokio::test]
        async fn varchar_plp_known_length_reports_its_collation_encoding() {
            let collation = SqlCollation::default();
            let md = plp_metadata(
                TdsDataType::BigVarChar,
                PartialLengthType::BigVarChar,
                Some(collation),
            );
            let payload = b"plain text";
            assert_eq!(
                decode_string(plp_known(payload, 3), &md, true).await,
                vec![sunk(payload, Some(EncodingType::LcidBased(collation)))]
            );
        }

        #[tokio::test]
        async fn ntext_long_len_never_requests_a_destination() {
            let md = varlen_metadata(TdsDataType::NText, 0x7FFFFFFF);
            let payload: Vec<u8> = "hello world"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            let mut wire = vec![16u8];
            wire.extend_from_slice(&[0u8; 16]); // textptr
            wire.extend_from_slice(&[0u8; 8]); // timestamp
            wire.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            wire.extend_from_slice(&payload);
            assert_eq!(
                decode_string(wire, &md, true).await,
                vec![Event::OwnedString(payload)]
            );
        }

        #[tokio::test]
        async fn ntext_null_text_pointer_never_requests_a_destination() {
            let md = varlen_metadata(TdsDataType::NText, 0x7FFFFFFF);
            assert_eq!(decode_string(vec![0u8], &md, true).await, vec![Event::Null]);
        }

        #[tokio::test]
        async fn string_sink_declined_falls_back_to_owned_path() {
            let md = plp_metadata(TdsDataType::NVarChar, PartialLengthType::NVarChar, None);
            let utf16: Vec<u8> = "abc".encode_utf16().flat_map(u16::to_le_bytes).collect();
            assert_eq!(
                decode_string(plp_known(&utf16, 2), &md, false).await,
                vec![Event::OwnedString(utf16)]
            );
        }

        // ---------------------------------------------------------------
        // Failure and short-fill semantics
        // ---------------------------------------------------------------

        #[tokio::test]
        async fn truncated_payload_commits_the_value_as_incomplete() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            // Declare 8 bytes and announce an 8-byte chunk, then supply three.
            let mut wire = 8u64.to_le_bytes().to_vec();
            wire.extend_from_slice(&8u32.to_le_bytes());
            wire.extend_from_slice(&[1, 2, 3]);

            let mut reader = ByteReader::new(wire);
            let mut writer = RecordingSink::new(true);
            let err = GenericDecoder::default()
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await;

            assert!(err.is_err());
            assert!(matches!(
                writer.events.as_slice(),
                [Event::Sunk {
                    complete: false,
                    ..
                }]
            ));
        }

        #[tokio::test]
        async fn plp_shorter_than_declared_length_zero_fills_the_remainder() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            // Declare 8 bytes but terminate the chunk stream after 3.
            let mut wire = 8u64.to_le_bytes().to_vec();
            wire.extend_from_slice(&3u32.to_le_bytes());
            wire.extend_from_slice(&[1, 2, 3]);
            wire.extend_from_slice(&0u32.to_le_bytes());

            let sink = decode_generic(wire.clone(), &md, true).await;
            let owned = decode_generic(wire, &md, false).await;

            assert_eq!(sink, vec![sunk(&[1, 2, 3, 0, 0, 0, 0, 0], None)]);
            assert_eq!(owned, vec![Event::OwnedBytes(vec![1, 2, 3, 0, 0, 0, 0, 0])]);
        }

        #[tokio::test]
        async fn plp_chunk_past_declared_length_is_rejected_on_both_paths() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            // Declare 2 bytes, then send a 4-byte chunk.
            let mut wire = 2u64.to_le_bytes().to_vec();
            wire.extend_from_slice(&4u32.to_le_bytes());
            wire.extend_from_slice(&[1, 2, 3, 4]);
            wire.extend_from_slice(&0u32.to_le_bytes());

            for accept in [true, false] {
                let mut reader = ByteReader::new(wire.clone());
                let mut writer = RecordingSink::new(accept);
                let err = GenericDecoder::default()
                    .decode_into(&mut reader, &md, 0, &mut writer)
                    .await;
                assert!(err.is_err(), "accept={accept} should have been rejected");
            }
        }

        #[tokio::test]
        #[should_panic(expected = "value_destination returned 1 bytes for a 2-byte value")]
        async fn wrong_sized_destination_is_rejected() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let mut reader = ByteReader::new(plp_known(&[1, 2], 2));
            let mut writer = RecordingSink::with_destination_len(1);
            GenericDecoder::default()
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn overlong_destination_does_not_relax_wire_declared_length() {
            let mut wire = 4u32.to_le_bytes().to_vec();
            wire.extend_from_slice(&[1, 2, 3, 4]);
            wire.extend_from_slice(&0u32.to_le_bytes());
            let mut reader = ByteReader::new(wire);
            let mut destination = [MaybeUninit::uninit(); 4];

            let err = GenericDecoder::read_plp_chunks_into_slice(&mut reader, &mut destination, 2)
                .await
                .unwrap_err();

            assert!(err.to_string().contains("wire_declared_len=2"));
        }

        #[tokio::test]
        async fn xml_json_udt_and_vector_never_request_destinations() {
            for (data_type, partial_type) in [
                (TdsDataType::Xml, PartialLengthType::Xml),
                (TdsDataType::Json, PartialLengthType::Json),
                (TdsDataType::Udt, PartialLengthType::Udt),
            ] {
                let md = plp_metadata(data_type, partial_type, None);
                let mut reader = ByteReader::new(plp_known(&[], 1));
                let mut writer = RecordingSink::new(true);
                let _ = GenericDecoder::default()
                    .decode_into(&mut reader, &md, 0, &mut writer)
                    .await;
                assert_eq!(
                    writer.destination_requests, 0,
                    "{data_type:?} requested a destination"
                );
            }

            let md = varlen_metadata(TdsDataType::Vector, 8);
            let mut reader = ByteReader::new(Vec::new());
            let mut writer = RecordingSink::new(true);
            let _ = GenericDecoder::default()
                .decode_into(&mut reader, &md, 0, &mut writer)
                .await;
            assert_eq!(writer.destination_requests, 0);
        }

        /// The sink path must not change what a writer that ignores it sees.
        #[tokio::test]
        async fn owned_path_matches_sink_path_byte_for_byte() {
            let md = plp_metadata(
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            );
            let payload: Vec<u8> = (0..200u8).map(|b| b.wrapping_mul(7)).collect();
            let wire = plp_known(&payload, 17);

            let Event::Sunk { bytes, .. } =
                decode_generic(wire.clone(), &md, true).await.pop().unwrap()
            else {
                panic!("expected a sunk value");
            };
            let Event::OwnedBytes(owned) = decode_generic(wire, &md, false).await.pop().unwrap()
            else {
                panic!("expected an owned value");
            };
            assert_eq!(bytes, owned);
        }
    }
}
