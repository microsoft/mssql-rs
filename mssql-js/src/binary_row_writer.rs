// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashMap;

use mssql_tds::datatypes::column_values::{
    SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
    SqlSmallMoney, SqlTime, SqlXml,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::row_writer::{LenHint, RowWriter, ValueKind};
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sql_vector::SqlVector;
use uuid::Uuid;

// Cell type tags — matched by decode.ts on the JS side.
const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_U8: u8 = 2;
const TAG_I16: u8 = 3;
const TAG_I32: u8 = 4;
const TAG_I64: u8 = 5;
const TAG_F32: u8 = 6;
const TAG_F64: u8 = 7;
const TAG_STRING_REF: u8 = 8;
const TAG_BYTES: u8 = 9;
const TAG_DECIMAL: u8 = 10;
const TAG_UUID: u8 = 11;
const TAG_DATE: u8 = 12;
const TAG_TIME: u8 = 13;
const TAG_DATETIME: u8 = 14;
const TAG_SMALLDATETIME: u8 = 15;
const TAG_DATETIME2: u8 = 16;
const TAG_DATETIMEOFFSET: u8 = 17;
const TAG_MONEY: u8 = 18;
const TAG_SMALLMONEY: u8 = 19;

/// Binary buffer format version.
const FORMAT_VERSION: u8 = 1;
/// Magic bytes "MSSQ".
const MAGIC: u32 = 0x4D535351;
/// Fixed header size: magic(4) + version(1) + col_count(2) + row_count(4) + string_table_offset(4) + rows_affected(4).
const HEADER_SIZE: usize = 19;

/// Encodes TDS row data into a compact binary buffer with string interning.
///
/// Buffer layout (written by `finalize`):
/// ```text
/// [Header 19B] [ColDescs 5B×N] [StringTable] [RowData]
/// ```
pub(crate) struct BinaryRowWriter {
    /// Cell data accumulated during streaming.
    row_data: Vec<u8>,
    /// Interned strings: UTF-8 content → index.
    string_map: HashMap<String, u32>,
    /// Ordered string entries (index = position in table).
    string_table: Vec<String>,
    /// Number of completed rows.
    row_count: u32,
    /// Column count for this result set.
    col_count: u16,
    /// Staging area for a character value being streamed in; blobs bypass this
    /// and are written straight into `row_data`.
    scratch: Vec<u8>,
    /// State of the value currently being streamed, if any.
    pending: Option<Pending>,
    /// Offset in `row_data` where the current row began, for [`RowWriter::abandon_row`].
    row_start: usize,
}

/// A variable-length value in the middle of being streamed in.
enum Pending {
    /// Character data staged in `scratch`, transcoded and interned on commit.
    Str(EncodingType),
    /// Binary data written directly into `row_data`; `len_pos` is the offset of the
    /// placeholder length that gets patched on commit.
    Blob { len_pos: usize },
}

impl BinaryRowWriter {
    pub fn new(col_count: u16) -> Self {
        Self {
            row_data: Vec::with_capacity(64 * 1024),
            string_map: HashMap::new(),
            string_table: Vec::new(),
            row_count: 0,
            col_count,
            scratch: Vec::new(),
            pending: None,
            row_start: 0,
        }
    }

    /// Intern a UTF-8 string and return its index in the string table.
    fn intern_string(&mut self, s: String) -> u32 {
        if let Some(&idx) = self.string_map.get(&s) {
            return idx;
        }
        let idx = self.string_table.len() as u32;
        self.string_map.insert(s.clone(), idx);
        self.string_table.push(s);
        idx
    }

    /// Intern column names from metadata and return the name indices.
    pub fn intern_column_names(&mut self, names: &[String]) -> Vec<u32> {
        names
            .iter()
            .map(|n| self.intern_string(n.clone()))
            .collect()
    }

    /// Assemble the final binary buffer.
    ///
    /// Layout:
    /// ```text
    /// Header:
    ///   magic(u32 LE) version(u8) col_count(u16 LE) row_count(u32 LE)
    ///   string_table_offset(u32 LE) rows_affected(i32 LE)
    /// Column descriptors × col_count:
    ///   name_string_idx(u32 LE) type_id(u8)
    /// String table:
    ///   entry_count(u32 LE)
    ///   [offset(u32 LE) len(u32 LE)] × entry_count
    ///   [utf8 bytes...]
    /// Row data:
    ///   [tag(u8) value...] per cell, col_count cells per row
    /// ```
    pub fn finalize(
        self,
        col_name_indices: &[u32],
        col_type_ids: &[u8],
        rows_affected: i32,
    ) -> Vec<u8> {
        // Build string table bytes
        let mut str_data = Vec::new();
        let mut str_offsets: Vec<(u32, u32)> = Vec::with_capacity(self.string_table.len());
        for s in &self.string_table {
            let offset = str_data.len() as u32;
            let bytes = s.as_bytes();
            str_data.extend_from_slice(bytes);
            str_offsets.push((offset, bytes.len() as u32));
        }

        let col_desc_size = (self.col_count as usize) * 5;
        let str_table_header = 4 + self.string_table.len() * 8; // entry_count + offsets
        let string_table_offset = (HEADER_SIZE + col_desc_size) as u32;
        let total =
            HEADER_SIZE + col_desc_size + str_table_header + str_data.len() + self.row_data.len();

        let mut buf = Vec::with_capacity(total);

        // Header
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.push(FORMAT_VERSION);
        buf.extend_from_slice(&self.col_count.to_le_bytes());
        buf.extend_from_slice(&self.row_count.to_le_bytes());
        buf.extend_from_slice(&string_table_offset.to_le_bytes());
        buf.extend_from_slice(&rows_affected.to_le_bytes());

        // Column descriptors
        for i in 0..self.col_count as usize {
            let name_idx = col_name_indices.get(i).copied().unwrap_or(0);
            let type_id = col_type_ids.get(i).copied().unwrap_or(0);
            buf.extend_from_slice(&name_idx.to_le_bytes());
            buf.push(type_id);
        }

        // String table
        buf.extend_from_slice(&(self.string_table.len() as u32).to_le_bytes());
        for (offset, len) in &str_offsets {
            buf.extend_from_slice(&offset.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }
        buf.extend_from_slice(&str_data);

        // Row data
        buf.extend_from_slice(&self.row_data);

        buf
    }
}

impl RowWriter for BinaryRowWriter {
    fn write_null(&mut self, _col: usize) {
        self.row_data.push(TAG_NULL);
    }

    fn write_bool(&mut self, _col: usize, val: bool) {
        self.row_data.push(TAG_BOOL);
        self.row_data.push(val as u8);
    }

    fn write_u8(&mut self, _col: usize, val: u8) {
        self.row_data.push(TAG_U8);
        self.row_data.push(val);
    }

    fn write_i16(&mut self, _col: usize, val: i16) {
        self.row_data.push(TAG_I16);
        self.row_data.extend_from_slice(&val.to_le_bytes());
    }

    fn write_i32(&mut self, _col: usize, val: i32) {
        self.row_data.push(TAG_I32);
        self.row_data.extend_from_slice(&val.to_le_bytes());
    }

    fn write_i64(&mut self, _col: usize, val: i64) {
        self.row_data.push(TAG_I64);
        self.row_data.extend_from_slice(&val.to_le_bytes());
    }

    fn write_f32(&mut self, _col: usize, val: f32) {
        self.row_data.push(TAG_F32);
        self.row_data.extend_from_slice(&val.to_le_bytes());
    }

    fn write_f64(&mut self, _col: usize, val: f64) {
        self.row_data.push(TAG_F64);
        self.row_data.extend_from_slice(&val.to_le_bytes());
    }

    fn write_str(&mut self, _col: usize, bytes: &[u8], encoding: EncodingType) {
        let utf8 = SqlString::new(bytes.to_vec(), encoding).to_utf8_string();
        let idx = self.intern_string(utf8);
        self.row_data.push(TAG_STRING_REF);
        self.row_data.extend_from_slice(&idx.to_le_bytes());
    }

    fn write_blob(&mut self, _col: usize, bytes: &[u8]) {
        self.row_data.push(TAG_BYTES);
        self.row_data
            .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.row_data.extend_from_slice(bytes);
    }

    fn write_decimal(&mut self, _col: usize, val: DecimalParts) {
        self.row_data.push(TAG_DECIMAL);
        self.row_data.push(val.is_positive as u8);
        self.row_data.push(val.scale);
        self.row_data.push(val.precision);
        self.row_data.push(val.int_parts.len() as u8);
        for part in &val.int_parts {
            self.row_data.extend_from_slice(&part.to_le_bytes());
        }
    }

    fn write_numeric(&mut self, _col: usize, val: DecimalParts) {
        // Same encoding as decimal
        self.row_data.push(TAG_DECIMAL);
        self.row_data.push(val.is_positive as u8);
        self.row_data.push(val.scale);
        self.row_data.push(val.precision);
        self.row_data.push(val.int_parts.len() as u8);
        for part in &val.int_parts {
            self.row_data.extend_from_slice(&part.to_le_bytes());
        }
    }

    fn write_date(&mut self, _col: usize, val: SqlDate) {
        self.row_data.push(TAG_DATE);
        self.row_data
            .extend_from_slice(&val.get_days().to_le_bytes());
    }

    fn write_time(&mut self, _col: usize, val: SqlTime) {
        self.row_data.push(TAG_TIME);
        self.row_data.push(val.scale);
        self.row_data
            .extend_from_slice(&val.time_nanoseconds.to_le_bytes());
    }

    fn write_datetime(&mut self, _col: usize, val: SqlDateTime) {
        self.row_data.push(TAG_DATETIME);
        self.row_data.extend_from_slice(&val.days.to_le_bytes());
        self.row_data.extend_from_slice(&val.time.to_le_bytes());
    }

    fn write_smalldatetime(&mut self, _col: usize, val: SqlSmallDateTime) {
        self.row_data.push(TAG_SMALLDATETIME);
        self.row_data.extend_from_slice(&val.days.to_le_bytes());
        self.row_data.extend_from_slice(&val.time.to_le_bytes());
    }

    fn write_datetime2(&mut self, _col: usize, val: SqlDateTime2) {
        self.row_data.push(TAG_DATETIME2);
        self.row_data.push(val.time.scale);
        self.row_data
            .extend_from_slice(&val.time.time_nanoseconds.to_le_bytes());
        self.row_data.extend_from_slice(&val.days.to_le_bytes());
    }

    fn write_datetimeoffset(&mut self, _col: usize, val: SqlDateTimeOffset) {
        self.row_data.push(TAG_DATETIMEOFFSET);
        self.row_data.push(val.datetime2.time.scale);
        self.row_data
            .extend_from_slice(&val.datetime2.time.time_nanoseconds.to_le_bytes());
        self.row_data
            .extend_from_slice(&val.datetime2.days.to_le_bytes());
        self.row_data.extend_from_slice(&val.offset.to_le_bytes());
    }

    fn write_money(&mut self, _col: usize, val: SqlMoney) {
        self.row_data.push(TAG_MONEY);
        self.row_data.extend_from_slice(&val.lsb_part.to_le_bytes());
        self.row_data.extend_from_slice(&val.msb_part.to_le_bytes());
    }

    fn write_smallmoney(&mut self, _col: usize, val: SqlSmallMoney) {
        self.row_data.push(TAG_SMALLMONEY);
        self.row_data.extend_from_slice(&val.int_val.to_le_bytes());
    }

    fn write_uuid(&mut self, _col: usize, val: Uuid) {
        self.row_data.push(TAG_UUID);
        self.row_data.extend_from_slice(val.as_bytes());
    }

    fn write_xml(&mut self, _col: usize, val: SqlXml) {
        let utf8 = val.as_string();
        let idx = self.intern_string(utf8);
        self.row_data.push(TAG_STRING_REF);
        self.row_data.extend_from_slice(&idx.to_le_bytes());
    }

    fn write_json(&mut self, _col: usize, val: SqlJson) {
        let utf8 = val.as_string();
        let idx = self.intern_string(utf8);
        self.row_data.push(TAG_STRING_REF);
        self.row_data.extend_from_slice(&idx.to_le_bytes());
    }

    fn write_vector(&mut self, _col: usize, val: SqlVector) {
        // Encode f32 vector as raw bytes
        self.row_data.push(TAG_BYTES);
        if let Some(floats) = val.as_f32() {
            let byte_len = (floats.len() * 4) as u32;
            self.row_data.extend_from_slice(&byte_len.to_le_bytes());
            for f in floats {
                self.row_data.extend_from_slice(&f.to_le_bytes());
            }
        } else {
            self.row_data.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    fn begin_value(&mut self, _col: usize, kind: ValueKind, hint: LenHint) {
        /// Cap on eager allocation; a PLP header may declare ~2 GiB before any payload
        /// arrives, so a declared length is a sizing hint, not an allocation order.
        const PREALLOC_CAP: usize = 1 << 20;
        let reserve_by = match hint {
            LenHint::Exact(n) => n.min(PREALLOC_CAP),
            LenHint::Unknown => 0,
        };
        self.pending = Some(match kind {
            ValueKind::Str(enc) => {
                self.scratch.reserve(reserve_by);
                Pending::Str(enc)
            }
            ValueKind::Blob => {
                // Emit the tag and a placeholder length now, then let the decoder write
                // payload bytes straight into row_data. Nothing is copied twice.
                self.row_data.push(TAG_BYTES);
                let len_pos = self.row_data.len();
                self.row_data.extend_from_slice(&0u32.to_le_bytes());
                self.row_data.reserve(reserve_by);
                Pending::Blob { len_pos }
            }
        });
    }

    fn reserve(&mut self, _col: usize, n: usize) -> &mut [u8] {
        let buf = match self.pending {
            Some(Pending::Blob { .. }) => &mut self.row_data,
            _ => &mut self.scratch,
        };
        let at = buf.len();
        // resize, not set_len: the decoder may read these bytes back, and handing out
        // uninitialized memory as &mut [u8] is undefined behaviour.
        buf.resize(at + n, 0);
        &mut buf[at..]
    }

    fn commit_value(&mut self, _col: usize) {
        match self
            .pending
            .take()
            .expect("commit_value without begin_value")
        {
            Pending::Str(enc) => {
                let utf8 = SqlString::new(std::mem::take(&mut self.scratch), enc).to_utf8_string();
                let idx = self.intern_string(utf8);
                self.row_data.push(TAG_STRING_REF);
                self.row_data.extend_from_slice(&idx.to_le_bytes());
            }
            Pending::Blob { len_pos } => {
                let len = (self.row_data.len() - len_pos - 4) as u32;
                self.row_data[len_pos..len_pos + 4].copy_from_slice(&len.to_le_bytes());
            }
        }
    }

    fn end_row(&mut self) {
        self.row_count += 1;
        self.row_start = self.row_data.len();
    }

    fn abandon_row(&mut self) {
        self.pending = None;
        self.scratch.clear();
        self.row_data.truncate(self.row_start);
    }
}

impl BinaryRowWriter {
    pub fn row_data_len(&self) -> usize {
        self.row_data.len()
    }
}
