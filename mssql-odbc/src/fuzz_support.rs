// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal parse/convert entry points re-exposed as safe wrappers for
//! coverage-guided fuzzing (`fuzz/`). Compiled only under `--cfg fuzzing`, which
//! cargo-fuzz sets, so the shipped driver's FFI surface is unchanged.
//!
//! Each wrapper drives one deterministic, side-effect-free decoder over
//! caller-controlled bytes — the same inputs an application feeds across the
//! ODBC boundary — and discards the decoded value. The fuzzer is hunting for
//! panics, hangs, and (for the pointer readers) out-of-bounds reads, not for a
//! particular result.

use crate::api::odbc_types::{
    SQL_ATTR_ODBC_VERSION, SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DATE, SQL_C_DOUBLE,
    SQL_C_FLOAT, SQL_C_GUID, SQL_C_LONG, SQL_C_NUMERIC, SQL_C_SBIGINT, SQL_C_SHORT, SQL_C_SLONG,
    SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT, SQL_C_STINYINT, SQL_C_TIME,
    SQL_C_TIMESTAMP, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP,
    SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_C_WCHAR, SQL_HANDLE_DBC,
    SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_NTS, SQL_NULL_DATA, SQL_NULL_HANDLE, SQL_OV_ODBC3_80,
    SQL_PARAM_INPUT, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SQL_VARCHAR, SQL_WVARCHAR, SqlHandle,
    SqlInteger, SqlLen, SqlPointer, SqlSmallInt, SqlULen, SqlUSmallInt, SqlWChar,
};
use crate::api::util::{read_utf16, read_utf16_attr, read_utf16_long};
use crate::api::{
    SQLAllocHandle, SQLExecDirectW, SQLFetch, SQLFreeHandle, SQLGetData, SQLNumResultCols,
    SQLSetEnvAttr,
};
use crate::connection::connection_string_parser::parse_connection_string;
use crate::conversion::fetch_convert::{
    convert_datetime_c, convert_float_c, convert_guid_c, convert_integer_c, is_datetime_c_target,
    is_float_c_target, is_integer_c_target, parse_date_literal, parse_datetime_literal,
    parse_time_literal,
};
use crate::conversion::numeric::{narrow_i128, parse_numeric_text};
use crate::conversion::param_convert::{bound_param_to_value, transcode_dae_bytes};
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, handle_from_raw};
use crate::params::BoundParam;
use crate::params::conversion_matrix::{
    BINARY_SQL_TARGETS, CHARACTER_SQL_TARGETS, INTEGER_SQL_TARGETS,
};
use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::fuzz_support::{FuzzPacketReader, create_fuzz_tds_client};
use mssql_tds::token::tokens::SqlCollation;
use std::ffi::c_void;
use std::sync::LazyLock;

/// Drive the `SQLDriverConnect` connection-string state machine, then the
/// redacted re-rendering path on any accepted parse.
pub fn fuzz_connection_string(input: &str) {
    if let Ok((params, _has_warnings)) = parse_connection_string(input) {
        let _ = params.fmt_as_odbc_conn_str();
    }
}

/// Drive character→numeric parsing and every downstream narrowing.
pub fn fuzz_numeric_text(input: &str) {
    let Ok(src) = parse_numeric_text(input) else {
        return;
    };
    let _ = src.as_f64();
    let _ = src.is_negative();
    if let Some((v, _dropped)) = src.to_i128_truncating() {
        let _ = narrow_i128::<i64>(v);
        let _ = narrow_i128::<i32>(v);
        let _ = narrow_i128::<i16>(v);
        let _ = narrow_i128::<i8>(v);
        let _ = narrow_i128::<u8>(v);
    }
}

/// Drive the character date / time / datetime literal parsers.
pub fn fuzz_datetime_literal(input: &str) {
    let _ = parse_date_literal(input);
    let _ = parse_time_literal(input);
    let _ = parse_datetime_literal(input);
}

/// Drive the data-at-execution transcoding path (`SQLPutData` buffered value)
/// across the C-type / SQL-type / collation matrix. The bytes are the streamed
/// parameter value; `mode` selects the encoding pairing the same way a bound
/// parameter's declared types would. Pure byte-to-byte transcoding, so any
/// panic here is a real defect on the input-parameter path.
pub fn fuzz_transcode_dae_bytes(input: &[u8], mode: u8) {
    let c_type = if mode & 1 == 0 {
        SQL_C_CHAR
    } else {
        SQL_C_WCHAR
    };
    let sql_type = if mode & 2 == 0 {
        SQL_VARCHAR
    } else {
        SQL_WVARCHAR
    };
    let collation = match (mode >> 2) % 3 {
        0 => SqlCollation {
            col_flags: 0x40, // fUTF8
            ..SqlCollation::default()
        },
        1 => SqlCollation {
            info: 0x0409, // US English LCID -> Windows-1252
            ..SqlCollation::default()
        },
        _ => SqlCollation::default(),
    };
    let _ = transcode_dae_bytes(c_type, sql_type, input.to_vec(), collation);
}

/// Drive the unsafe UTF-16 buffer readers over a real, bounded slice.
///
/// The slice supplies the backing store, so the pointer is always valid for
/// `units.len()` `SQLWCHAR`s — exactly the invariant the readers' `# Safety`
/// contracts require. Explicit lengths never exceed the slice, and the
/// NUL-terminated (`SQL_NTS`) path is taken only when the buffer actually holds
/// a terminator, so its scan cannot run past the end.
pub fn fuzz_read_utf16(units: &[u16], mode: u8) {
    let ptr: *const SqlWChar = units.as_ptr();
    match mode % 4 {
        0 => {
            let len = SqlSmallInt::try_from(units.len()).unwrap_or(SqlSmallInt::MAX);
            let _ = unsafe { read_utf16(ptr, len) };
        }
        1 => {
            let len = SqlInteger::try_from(units.len()).unwrap_or(SqlInteger::MAX);
            let _ = unsafe { read_utf16_long(ptr, len) };
        }
        2 => {
            let byte_len =
                SqlInteger::try_from(units.len().saturating_mul(2)).unwrap_or(SqlInteger::MAX);
            let _ = unsafe { read_utf16_attr(ptr, byte_len) };
        }
        _ => {
            if units.contains(&0) {
                let _ = unsafe { read_utf16(ptr, SQL_NTS) };
            } else {
                let len = SqlSmallInt::try_from(units.len()).unwrap_or(SqlSmallInt::MAX);
                let _ = unsafe { read_utf16(ptr, len) };
            }
        }
    }
}

/// A forgiving little-endian byte reader. Short reads zero-fill rather than
/// fail, so any input length yields a value and the fuzzer never wastes an
/// execution on a length check.
struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        let end = self.pos.saturating_add(n).min(self.data.len());
        let mut v = self.data.get(self.pos..end).unwrap_or(&[]).to_vec();
        self.pos = end;
        v.resize(n, 0);
        v
    }

    fn arr<const N: usize>(&mut self) -> [u8; N] {
        self.take(N).try_into().unwrap_or([0u8; N])
    }

    fn rest(&mut self) -> Vec<u8> {
        let v = self.data.get(self.pos..).unwrap_or(&[]).to_vec();
        self.pos = self.data.len();
        v
    }
}

/// Builds a numeric literal from raw bytes so `DecimalParts::from_string` mostly
/// accepts it, exercising the decimal decode + downstream narrowing.
fn build_decimal(cur: &mut ByteCursor) -> Option<DecimalParts> {
    let ndigits = (cur.u8() % 39) as usize; // 0..=38
    let scale = cur.u8() % 39;
    let mut s = String::new();
    if cur.u8() & 1 == 0 {
        s.push('-');
    }
    for _ in 0..=ndigits {
        s.push(char::from(b'0' + (cur.u8() % 10)));
    }
    let precision = (ndigits as u8 + 1).clamp(1, 38);
    DecimalParts::from_string(&s, precision, scale.min(precision)).ok()
}

/// Selects a realistic fetch-side string encoding. Deliberately excludes
/// `DelayedSet` (a param-staging placeholder that never labels a fetched
/// column) so a manufactured-impossible state can't masquerade as a defect.
fn build_encoding(cur: &mut ByteCursor) -> EncodingType {
    match cur.u8() % 3 {
        0 => EncodingType::Utf8,
        1 => EncodingType::Utf16,
        _ => EncodingType::LcidBased(SqlCollation {
            info: u32::from_le_bytes(cur.arr()),
            lcid_language_id: 0,
            col_flags: cur.u8(),
            sort_id: cur.u8(),
        }),
    }
}

/// Assembles a `ColumnValues` from fuzz bytes, covering the numeric, temporal,
/// money, character, and binary variants the fetch converters actually read.
fn build_column_value(cur: &mut ByteCursor) -> ColumnValues {
    match cur.u8() % 21 {
        0 => ColumnValues::TinyInt(cur.u8()),
        1 => ColumnValues::SmallInt(i16::from_le_bytes(cur.arr())),
        2 => ColumnValues::Int(i32::from_le_bytes(cur.arr())),
        3 => ColumnValues::BigInt(i64::from_le_bytes(cur.arr())),
        4 => ColumnValues::Real(f32::from_le_bytes(cur.arr())),
        5 => ColumnValues::Float(f64::from_le_bytes(cur.arr())),
        6 => ColumnValues::Bit(cur.u8() & 1 == 1),
        7 => build_decimal(cur).map_or(ColumnValues::Null, ColumnValues::Decimal),
        8 => build_decimal(cur).map_or(ColumnValues::Null, ColumnValues::Numeric),
        9 => ColumnValues::Money(SqlMoney {
            lsb_part: i32::from_le_bytes(cur.arr()),
            msb_part: i32::from_le_bytes(cur.arr()),
        }),
        10 => ColumnValues::SmallMoney(SqlSmallMoney {
            int_val: i32::from_le_bytes(cur.arr()),
        }),
        11 => {
            let enc = build_encoding(cur);
            ColumnValues::String(SqlString::new(cur.rest(), enc))
        }
        12 => {
            let days = u32::from_le_bytes(cur.arr()) % 3_652_059;
            SqlDate::create(days).map_or(ColumnValues::Null, ColumnValues::Date)
        }
        13 => ColumnValues::Time(SqlTime {
            time_nanoseconds: u64::from_le_bytes(cur.arr()),
            scale: cur.u8(),
        }),
        14 => ColumnValues::DateTime2(SqlDateTime2 {
            days: u32::from_le_bytes(cur.arr()),
            time: SqlTime {
                time_nanoseconds: u64::from_le_bytes(cur.arr()),
                scale: cur.u8(),
            },
        }),
        15 => ColumnValues::DateTimeOffset(SqlDateTimeOffset {
            datetime2: SqlDateTime2 {
                days: u32::from_le_bytes(cur.arr()),
                time: SqlTime {
                    time_nanoseconds: u64::from_le_bytes(cur.arr()),
                    scale: cur.u8(),
                },
            },
            offset: i16::from_le_bytes(cur.arr()),
        }),
        16 => ColumnValues::DateTime(SqlDateTime {
            days: i32::from_le_bytes(cur.arr()),
            time: u32::from_le_bytes(cur.arr()),
        }),
        17 => ColumnValues::SmallDateTime(SqlSmallDateTime {
            days: u16::from_le_bytes(cur.arr()),
            time: u16::from_le_bytes(cur.arr()),
        }),
        18 => ColumnValues::Bytes(cur.rest()),
        19 => ColumnValues::Uuid(uuid::Uuid::from_bytes(cur.arr())),
        _ => ColumnValues::Null,
    }
}

/// Fixed-width `SQL_C_*` fetch targets offered to the conversion fuzzers. The
/// reachable subset is derived at run time by [`routes_fixed_width_fetch`] — the
/// exact predicates `get_data::convert_typed_c` routes on — so the fuzzer tracks
/// production's routing table instead of a parallel list that can fall behind
/// it. A candidate production does not route yet (e.g. `SQL_C_NUMERIC`) is
/// filtered out and becomes reachable for free once a converter for it lands.
const FETCH_TARGET_CANDIDATES: [SqlSmallInt; 24] = [
    // is_integer_c_target: type_rules::is_integer_c_type + SQL_C_BIT
    SQL_C_STINYINT,
    SQL_C_TINYINT,
    SQL_C_UTINYINT,
    SQL_C_SSHORT,
    SQL_C_SHORT,
    SQL_C_USHORT,
    SQL_C_SLONG,
    SQL_C_LONG,
    SQL_C_ULONG,
    SQL_C_SBIGINT,
    SQL_C_UBIGINT,
    SQL_C_BIT,
    // is_float_c_target
    SQL_C_FLOAT,
    SQL_C_DOUBLE,
    // SQL_C_GUID
    SQL_C_GUID,
    // is_datetime_c_target
    SQL_C_TYPE_DATE,
    SQL_C_DATE,
    SQL_C_TYPE_TIME,
    SQL_C_TIME,
    SQL_C_SS_TIME2,
    SQL_C_TYPE_TIMESTAMP,
    SQL_C_TIMESTAMP,
    SQL_C_SS_TIMESTAMPOFFSET,
    // Implemented C type with no fetch converter yet — filtered out below until
    // production starts routing it.
    SQL_C_NUMERIC,
];

/// Exactly the targets `get_data::convert_typed_c` hands to a fixed-width
/// converter, spelled with the same predicates it routes on.
fn routes_fixed_width_fetch(target: SqlSmallInt) -> bool {
    is_integer_c_target(target)
        || is_float_c_target(target)
        || target == SQL_C_GUID
        || is_datetime_c_target(target)
}

/// The routed fixed-width targets, derived once from [`FETCH_TARGET_CANDIDATES`].
static ROUTED_FETCH_TARGETS: LazyLock<Vec<SqlSmallInt>> = LazyLock::new(|| {
    FETCH_TARGET_CANDIDATES
        .into_iter()
        .filter(|&t| routes_fixed_width_fetch(t))
        .collect()
});

/// Targets the FFI `SQLGetData` loop retrieves columns as: the routed
/// fixed-width set plus the variable-length character and binary targets, so
/// `SQLGetData`'s length and truncation bookkeeping is exercised alongside the
/// fixed-width writers.
static FFI_GETDATA_TARGETS: LazyLock<Vec<SqlSmallInt>> = LazyLock::new(|| {
    let mut targets = ROUTED_FETCH_TARGETS.clone();
    targets.extend_from_slice(&[SQL_C_CHAR, SQL_C_WCHAR, SQL_C_BINARY]);
    targets
});

/// Drive the result-fetch conversion matrix: a decoded column value written to
/// a fixed-width `SQL_C_*` target. Mirrors `get_data::convert_typed_c`'s routing
/// (integer / float / GUID / datetime) without needing the private `get_data`
/// module. The 64-byte target overtops every fixed C struct (the widest is the
/// 16-byte timestamp / GUID), and `write_fixed` writes unaligned, so the plain
/// `[u8]` backing is a valid write target. Hunts for arithmetic panics in the
/// numeric narrowing and the calendar/clock extraction math.
pub fn fuzz_fetch_convert(data: &[u8]) {
    let mut cur = ByteCursor::new(data);
    let target = ROUTED_FETCH_TARGETS[(cur.u8() as usize) % ROUTED_FETCH_TARGETS.len()];
    let value = build_column_value(&mut cur);

    let mut buf = [0u8; 64];
    let mut ind: SqlLen = 0;
    let ptr = buf.as_mut_ptr() as SqlPointer;
    let ind_ptr = &mut ind as *mut SqlLen;
    unsafe {
        let _ = if is_integer_c_target(target) {
            convert_integer_c(&value, target, ptr, ind_ptr)
        } else if is_float_c_target(target) {
            convert_float_c(&value, target, ptr, ind_ptr)
        } else if target == SQL_C_GUID {
            convert_guid_c(&value, target, ptr, ind_ptr)
        } else {
            convert_datetime_c(&value, target, ptr, ind_ptr)
        };
    }
}

/// Every C type `params::conversion_matrix` has a row for: the character and
/// binary buffers plus every `type_rules::is_integer_c_type` variant, including
/// the legacy `SQL_C_TINYINT`/`SQL_C_SHORT`/`SQL_C_LONG` spellings. `sql_type`
/// is drawn independently, so unsupported pairings still exercise the bind-time
/// rejection path.
const PARAM_C_TYPES: [SqlSmallInt; 14] = [
    SQL_C_CHAR,
    SQL_C_WCHAR,
    SQL_C_BINARY,
    SQL_C_STINYINT,
    SQL_C_TINYINT,
    SQL_C_UTINYINT,
    SQL_C_SSHORT,
    SQL_C_SHORT,
    SQL_C_USHORT,
    SQL_C_SLONG,
    SQL_C_LONG,
    SQL_C_ULONG,
    SQL_C_SBIGINT,
    SQL_C_UBIGINT,
];

/// Every SQL target `params::conversion_matrix::is_supported_conversion` can
/// reach, taken straight from the matrix's own target rows so the fuzzed set
/// follows the implemented conversions instead of a hand-copied list.
static PARAM_SQL_TYPES: LazyLock<Vec<SqlSmallInt>> = LazyLock::new(|| {
    [
        CHARACTER_SQL_TARGETS,
        BINARY_SQL_TARGETS,
        INTEGER_SQL_TARGETS,
    ]
    .concat()
});

/// Drive the bind-parameter read + convert path (`bound_param_to_value`) that
/// `SQLExecute` runs over an application's value and indicator buffers.
///
/// The value buffer is the fuzz input plus eight trailing zero bytes; that
/// padding guarantees a NUL terminator for the `SQL_NTS` scans and at least
/// eight readable bytes for the fixed-width integer reads, so every in-bounds
/// contract `read_param_value` documents holds and a crash reflects a real
/// conversion defect, not harness-induced UB. The indicator is confined to
/// values that keep the read inside the buffer.
pub fn fuzz_bound_param(data: &[u8]) {
    let mut cur = ByteCursor::new(data);
    let c_type = PARAM_C_TYPES[(cur.u8() as usize) % PARAM_C_TYPES.len()];
    let sql_type = PARAM_SQL_TYPES[(cur.u8() as usize) % PARAM_SQL_TYPES.len()];
    let ind_mode = cur.u8();
    let column_size = cur.u8() as SqlULen;
    let decimal_digits = SqlSmallInt::from(cur.u8() % 39);

    let mut value_buf = cur.rest();
    let fuzz_len = value_buf.len() as SqlLen;
    value_buf.extend_from_slice(&[0u8; 8]);

    let mut ind: SqlLen = match ind_mode % 4 {
        0 => SQL_NULL_DATA,
        1 => SQL_NTS as SqlLen,
        2 => fuzz_len,
        _ => fuzz_len / 2,
    };
    let ind_ptr = &mut ind as *mut SqlLen;
    let value_ptr = value_buf.as_mut_ptr() as *mut c_void;

    let param = BoundParam {
        input_output_type: SQL_PARAM_INPUT,
        c_type,
        sql_type,
        column_size,
        decimal_digits,
        parameter_value_ptr: value_ptr,
        buffer_length: fuzz_len,
        strlen_or_ind_ptr: ind_ptr,
        octet_length_ptr: ind_ptr,
    };
    let _ = unsafe { bound_param_to_value(&param) };
}

/// Drive the real ODBC result path end to end — `SQLExecDirectW` → `SQLFetch` →
/// `SQLGetData` — over a fuzzer-controlled TDS response stream.
///
/// A fresh ENV+DBC is built per call through the crate's own handle allocators,
/// then an in-memory `TdsClient` reading `data` (via `mssql-tds`'s
/// [`create_fuzz_tds_client`]) is installed as the connection — the same seam
/// [`crate::test_support::connect_mock_server`] uses, minus the socket. The
/// driver then executes a fixed batch (the text is irrelevant; the mock
/// transport replays `data` as the server's answer regardless) and walks every
/// row and column, so the fuzzer explores the whole COLMETADATA → ROW → convert
/// pipeline the way a hostile or corrupt server would feed it.
///
/// This crosses the `extern "C"` boundary the shipped driver exposes. Each
/// `SQL*` body is wrapped in `ffi_entry!`, which converts a Rust panic into
/// `SQL_ERROR` so it never unwinds into C; a genuine memory error is still
/// caught by the sanitizer, an unbounded read by the timeout, and a runaway
/// allocation by the RSS limit. Fresh handles per call keep every crash
/// reproducible from a single input rather than depending on residue from an
/// earlier one.
pub fn fuzz_ffi_execute(data: &[u8]) {
    if data.len() > 4096 {
        return;
    }
    let Some((&getdata_sel, server_bytes)) = data.split_first() else {
        return;
    };
    let target_type = FFI_GETDATA_TARGETS[getdata_sel as usize % FFI_GETDATA_TARGETS.len()];

    let mut env: SqlHandle = SQL_NULL_HANDLE;
    if unsafe { SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) } != SQL_SUCCESS {
        return;
    }
    unsafe {
        SQLSetEnvAttr(
            env,
            SQL_ATTR_ODBC_VERSION,
            SQL_OV_ODBC3_80 as usize as SqlPointer,
            0 as SqlInteger,
        );
    }

    let mut dbc: SqlHandle = SQL_NULL_HANDLE;
    if unsafe { SQLAllocHandle(SQL_HANDLE_DBC, env, &mut dbc) } == SQL_SUCCESS {
        install_fuzz_client(dbc, server_bytes);
        run_exec_fetch(dbc, target_type);
        unsafe { SQLFreeHandle(SQL_HANDLE_DBC, dbc) };
    }
    unsafe { SQLFreeHandle(SQL_HANDLE_ENV, env) };
}

/// Installs an in-memory, already-connected `TdsClient` over `server_bytes` on
/// `dbc`, so the execute path reads its response from the fuzz input instead of
/// a socket.
fn install_fuzz_client(dbc: SqlHandle, server_bytes: &[u8]) {
    let client = create_fuzz_tds_client(FuzzPacketReader::from_data(server_bytes), 4096);
    let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(dbc) };
    let mut state = dbc_ref.inner.lock().unwrap();
    state.client = Some(client);
    state.connection_state = ConnectionState::Connected;
}

/// Allocates a statement, executes a fixed batch, and drains every row/column
/// through `SQLGetData`. Row and column counts are capped so a fuzzed
/// COLMETADATA can't turn one input into an unbounded walk; the mock transport
/// EOFs when `server_bytes` runs out, which ends the fetch loop on its own.
fn run_exec_fetch(dbc: SqlHandle, target_type: SqlSmallInt) {
    let mut stmt: SqlHandle = SQL_NULL_HANDLE;
    if unsafe { SQLAllocHandle(SQL_HANDLE_STMT, dbc, &mut stmt) } != SQL_SUCCESS {
        return;
    }

    let query: Vec<SqlWChar> = "SELECT 1".encode_utf16().collect();
    let rc = unsafe { SQLExecDirectW(stmt, query.as_ptr(), query.len() as SqlSmallInt) };
    if rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO {
        let mut ncols: SqlSmallInt = 0;
        if unsafe { SQLNumResultCols(stmt, &mut ncols) } == SQL_SUCCESS {
            let cols = ncols.clamp(0, 64) as SqlUSmallInt;
            let mut buf = [0u8; 256];
            for _ in 0..128 {
                let fetch_rc = unsafe { SQLFetch(stmt) };
                if fetch_rc != SQL_SUCCESS && fetch_rc != SQL_SUCCESS_WITH_INFO {
                    break;
                }
                for col in 1..=cols {
                    let mut ind: SqlLen = 0;
                    let _ = unsafe {
                        SQLGetData(
                            stmt,
                            col,
                            target_type,
                            buf.as_mut_ptr() as SqlPointer,
                            buf.len() as SqlLen,
                            &mut ind,
                        )
                    };
                }
            }
        }
    }

    unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
}
