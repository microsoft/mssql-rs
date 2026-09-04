// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conversion from a bound application parameter buffer (`BoundParam`) to a
//! TDS RPC parameter (`RpcParameter`).
//!
//! Which C/SQL pairings reach this module is decided at bind time by
//! [`crate::api::type_rules`] and [`crate::params::conversion_matrix`];
//! `SQL_C_DEFAULT` has already been resolved to a concrete C type by then.
//! A data-at-execution parameter is declared from its SQL type, not its C
//! type: a PLP-able target streams, and anything else is collected and
//! converted whole through the ordinary materializing path -- see
//! [`dae_plan`]. A wideness mismatch (`SQL_C_WCHAR` streamed against a narrow
//! SQL type, or the reverse) is transcoded chunk by chunk, carrying a
//! character split across two `SQLPutData` calls -- see [`DaeTranscode`].
//! `SQL_DEFAULT_PARAM` is rejected with `07S01`, and an invalid negative
//! `StrLen_or_Ind` with `HY090`.
//!
//! A `SQL_NULL_DATA` parameter is materialised as a typed TDS NULL from
//! `sql_type` -- see [`typed_null`].

use std::borrow::Cow;

use mssql_tds::datatypes::sql_string::{EncodingType, SqlString, encode_narrow};
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{
    RpcParameter, RpcTypeMetadata, StatusFlags, StreamedSqlType,
};
use mssql_tds::token::tokens::SqlCollation;

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_C_BINARY, SQL_C_CHAR, SQL_C_WCHAR, SQL_CHAR,
    SQL_DATA_AT_EXEC, SQL_DECIMAL, SQL_DOUBLE, SQL_FLOAT, SQL_GUID, SQL_INTEGER,
    SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NULL_DATA, SQL_NUMERIC,
    SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SS_VARIANT, SQL_SS_VECTOR,
    SQL_SS_VECTOR_ELEMENT_SIZE, SQL_SS_XML, SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIME,
    SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR,
    SqlLen, SqlSmallInt, SqlSsVectorLayout,
};
use crate::api::sqlstate::{
    DiagMsg, ERR_DATA_AT_EXEC_NOT_STAGED, ERR_INVALID_CHARACTER_VALUE, ERR_INVALID_NULL_POINTER,
    ERR_INVALID_PARAM_PRECISION_OR_SCALE, ERR_INVALID_STRING_OR_BUFFER_LENGTH,
    ERR_INVALID_USE_OF_DEFAULT_PARAM, ERR_NUMERIC_OUT_OF_RANGE, ERR_PARAM_C_TYPE_NOT_IMPLEMENTED,
    ERR_PARAM_CONVERSION_NOT_IMPLEMENTED, ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED,
    ERR_PARAM_STRING_TRUNCATION, ERR_RESTRICTED_DATA_TYPE,
};
use crate::api::type_rules::{
    SQL_PREC_BIGCHARBINARY, SQL_PREC_NCHAR, SQL_PREC_NTEXT, SQL_PREC_NUMERIC, SQL_PREC_TEXTIMAGE,
    is_wide_character_sql_type,
};
use crate::conversion::error::ConvError;
use crate::conversion::numeric::{narrow_i128, parse_numeric_text};
use crate::conversion::param_buffer::{AppValue, Indicator, read_indicator, read_param_value};
use crate::params::BoundParam;

/// Why a bound parameter could not be turned into an RPC parameter.
///
/// Each variant carries its own SQLSTATE through [`diag`]. All but [`Value`]
/// describe the binding rather than the data; [`Value`] wraps a
/// [`crate::conversion::error::ConvError`] from the shared value model.
///
/// [`diag`]: ParamBuildError::diag
/// [`Value`]: ParamBuildError::Value
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamBuildError {
    /// Backstop only: bind time rejects any C type the conversion matrix does
    /// not list, so reaching this means the matrix and this module disagree.
    UnsupportedCType(SqlSmallInt),
    /// Backstop only: data-at-execution is supported, but `SQLExecute` and
    /// `SQLExecDirect` stage those parameters as streaming placeholders before
    /// reaching this module. An indicator that survives to here means staging
    /// missed the parameter, so the value buffer holds the application's token
    /// rather than data. Reported as a driver error, not "not implemented".
    DataAtExecNotStaged,
    /// `StrLen_or_Ind` was `SQL_DEFAULT_PARAM` on a statement that is not a
    /// canonical procedure call.
    InvalidUseOfDefaultParam,
    /// `StrLen_or_Ind` is a negative value that is not a valid input length.
    InvalidLength(SqlLen),
    /// The parameter carries a value but `ParameterValuePtr` is null.
    NullValuePointer,
    /// Character data longer than the declared length, in more than trailing
    /// blanks.
    StringTruncation,
    /// `ColumnSize` cannot be expressed as a T-SQL declaration for `SqlType`.
    InvalidParameterSize(usize),
    /// `DecimalDigits` cannot be expressed as a T-SQL scale for `SqlType`.
    InvalidDecimalDigits(SqlSmallInt),
    /// The SQL type cannot be materialised as a typed NULL.
    UnsupportedSqlType(SqlSmallInt),
    /// Backstop only: the C and SQL families are both known but the pairing
    /// between them is not built yet, so the matrix and this module disagree.
    ConversionNotImplemented,
    /// The value could not be represented in the target SQL type.
    Value(ConvError),
}

impl ParamBuildError {
    pub(crate) fn diag(self) -> DiagMsg {
        match self {
            Self::UnsupportedCType(_) => ERR_PARAM_C_TYPE_NOT_IMPLEMENTED,
            Self::DataAtExecNotStaged => ERR_DATA_AT_EXEC_NOT_STAGED,
            Self::InvalidUseOfDefaultParam => ERR_INVALID_USE_OF_DEFAULT_PARAM,
            Self::InvalidLength(_) => ERR_INVALID_STRING_OR_BUFFER_LENGTH,
            Self::NullValuePointer => ERR_INVALID_NULL_POINTER,
            Self::StringTruncation => ERR_PARAM_STRING_TRUNCATION,
            Self::InvalidParameterSize(_) | Self::InvalidDecimalDigits(_) => {
                ERR_INVALID_PARAM_PRECISION_OR_SCALE
            }
            Self::UnsupportedSqlType(_) => ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED,
            Self::ConversionNotImplemented => ERR_PARAM_CONVERSION_NOT_IMPLEMENTED,
            Self::Value(ConvError::OutOfRange) => ERR_NUMERIC_OUT_OF_RANGE,
            Self::Value(ConvError::InvalidCharacterValue) => ERR_INVALID_CHARACTER_VALUE,
            // Backstop only: parameter legality is settled by the bind-time
            // matrix, and `NotHandledHere` is an internal routing signal that is
            // never meant to reach an application.
            Self::Value(ConvError::Restricted | ConvError::NotHandledHere) => {
                ERR_RESTRICTED_DATA_TYPE
            }
        }
    }
}

/// Converts a bound parameter into a named (`@P1`-style) RPC parameter.
///
/// # Safety
/// See [`bound_param_to_value`].
pub(crate) unsafe fn bound_param_to_rpc(
    name: String,
    param: &BoundParam,
) -> Result<RpcParameter, ParamBuildError> {
    let (value, type_metadata) = unsafe { bound_param_to_value(param) }?;
    let parameter = RpcParameter::new(Some(name), StatusFlags::NONE, value);
    Ok(match type_metadata {
        Some(metadata) => parameter.with_type_metadata(metadata),
        None => parameter,
    })
}

/// Reads the application's value buffer and produces the corresponding
/// [`SqlType`].
///
/// # Safety
/// `param.parameter_value_ptr` and `param.strlen_or_ind_ptr` must satisfy the
/// ODBC binding contract: the value buffer is readable for the indicated
/// length and the indicator pointer, if non-null, points to one valid `SqlLen`.
pub(crate) unsafe fn bound_param_to_value(
    param: &BoundParam,
) -> Result<TypedValue, ParamBuildError> {
    // NULL is settled from the indicator alone, so a typed NULL never reads the
    // value buffer.
    let len_spec = match unsafe { read_indicator(param) }? {
        Indicator::Null => {
            return typed_null(param.sql_type, param.column_size, param.decimal_digits);
        }
        Indicator::Length(len) => len,
    };

    let app_value = unsafe { read_param_value(param, len_spec) }?;
    let family =
        sql_family(param.sql_type).ok_or(ParamBuildError::UnsupportedSqlType(param.sql_type))?;

    let value = match (app_value, family) {
        (AppValue::Integer(v), SqlFamily::Integer) => convert_integer_sql(param.sql_type, v)?,
        (AppValue::NarrowText(bytes), SqlFamily::Character) => {
            convert_character_sql(param.sql_type, param.column_size, AppText::Utf8(bytes))?
        }
        (AppValue::WideText(bytes), SqlFamily::Character) => {
            convert_character_sql(param.sql_type, param.column_size, AppText::Utf16(bytes))?
        }
        (AppValue::Binary(bytes), SqlFamily::Binary) => {
            convert_binary_sql(param.sql_type, param.column_size, bytes)?
        }
        (AppValue::Integer(v), SqlFamily::Character) => {
            convert_character_sql(param.sql_type, param.column_size, integer_as_text(v))?
        }
        (AppValue::NarrowText(bytes), SqlFamily::Integer) => {
            integer_from_text(param.sql_type, AppText::Utf8(bytes))?
        }
        (AppValue::WideText(bytes), SqlFamily::Integer) => {
            integer_from_text(param.sql_type, AppText::Utf16(bytes))?
        }
        (
            AppValue::Integer(_) | AppValue::NarrowText(_) | AppValue::WideText(_),
            SqlFamily::Binary,
        )
        | (AppValue::Binary(_), SqlFamily::Integer | SqlFamily::Character) => {
            return Err(ParamBuildError::ConversionNotImplemented);
        }
    };

    Ok((value, None))
}

/// Returns `true` when `indicator` is a data-at-execution value
/// (`SQL_DATA_AT_EXEC` or any value at or below `SQL_LEN_DATA_AT_EXEC_OFFSET`).
pub(crate) fn is_data_at_exec_indicator(indicator: SqlLen) -> bool {
    indicator == SQL_DATA_AT_EXEC || indicator <= SQL_LEN_DATA_AT_EXEC_OFFSET
}

/// UTF-16 code units contributed by a single UTF-8 byte.
///
/// Classified from the byte alone, which is what lets the bound below survive a
/// character split across two `SQLPutData` calls without carrying state: a
/// continuation byte belongs to a character already counted at its lead, and a
/// four-byte lead is exactly the one that decodes to a surrogate pair. A
/// sequence broken across chunks therefore has its lead counted in the first and
/// its continuations counted as nothing in the second, for the same total.
fn utf16_units_of_utf8_byte(byte: u8) -> usize {
    if byte & 0b1100_0000 == 0b1000_0000 {
        0
    } else if byte & 0b1111_1000 == 0b1111_0000 {
        2
    } else {
        1
    }
}

/// The unit a declaration's `ColumnSize` is counted in, and how much of it fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaeBound {
    /// Application-buffer bytes. Binary counts them directly, and a
    /// `SQL_C_WCHAR` character is a fixed two of them, so both are exact.
    Bytes(usize),
    /// UTF-16 code units of a UTF-8 buffer. `SQL_C_CHAR` bytes are not
    /// characters, and the materialized path measures every character input in
    /// UTF-16 units (`trim_blank_overflow`), so counting bytes here would reject
    /// a value -- one non-ASCII character against `varchar(1)` -- that the same
    /// binding materialized accepts. The unit is an approximation of collation
    /// bytes on both paths; making it exact is AB#47584.
    Utf16Units(usize),
}

/// How much a buffered parameter may accept, and what its overflow is allowed
/// to be made of.
///
/// Only produced for a same-family pairing, where the C type fixes the unit
/// unambiguously; a cross-family value's units do not correspond, so its bound
/// is left to the close-time converter instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaeLengthLimit {
    /// Maximum total the parameter may accept, in the unit named by the bound.
    bound: DaeBound,
    /// One unit of padding whose overflow is dropped rather than reported:
    /// `0x00` for binary, a blank for character, in the buffer's own encoding.
    pad_unit: &'static [u8],
}

impl DaeLengthLimit {
    /// Applies the limit to one chunk given the total already accepted,
    /// returning the prefix that may be kept and the units it consumed.
    ///
    /// `already` and the returned count are both in the bound's own unit, so a
    /// caller accumulates the return value rather than the kept byte length.
    ///
    /// Mirrors every same-family arm of msodbcsql's `SQLPutData`
    /// (`sqlccmd.cpp:10985-11218`): the bound is compared against the
    /// *accumulated* length rather than the chunk alone, an overflow made
    /// entirely of padding is trimmed away, and any other overflow is `22001`.
    /// Applying it here rather than at close is what puts the diagnostic on the
    /// same call msodbcsql puts it on.
    pub(crate) fn fit<'a>(
        &self,
        chunk: &'a [u8],
        already: usize,
    ) -> Result<(&'a [u8], usize), ParamBuildError> {
        let (split, consumed) = match self.bound {
            DaeBound::Bytes(max) => {
                let keep = max.saturating_sub(already);
                if chunk.len() <= keep {
                    return Ok((chunk, chunk.len()));
                }
                (keep, keep)
            }
            DaeBound::Utf16Units(max) => {
                let keep = max.saturating_sub(already);
                let mut units = 0;
                let mut split = chunk.len();
                for (i, &byte) in chunk.iter().enumerate() {
                    let cost = utf16_units_of_utf8_byte(byte);
                    // A continuation byte costs nothing, so it always stays with
                    // the lead that paid for it rather than starting an overflow
                    // of its own.
                    if units + cost > keep {
                        split = i;
                        break;
                    }
                    units += cost;
                }
                if split == chunk.len() {
                    return Ok((chunk, units));
                }
                (split, units)
            }
        };

        let overflow = &chunk[split..];
        // A trailing unit the chunk ended part-way through cannot be judged yet:
        // for `SQL_C_WCHAR` a lone `0x20` is the first half of a blank that the
        // next `SQLPutData` completes with `0x00`. Rejecting it here would fail
        // a value whose overflow is entirely padding purely because a chunk
        // boundary fell inside the pad unit -- and this change explicitly
        // supports a chunk splitting a UTF-16 code unit elsewhere. A partial
        // unit that is a prefix of the pad is therefore trimmed with the rest of
        // the padding; msodbcsql reaches the same outcome by masking an odd byte
        // count off entirely before it measures (`cbValue &= ~1`,
        // `odbc/sqlccmd.cpp:10931`).
        let unit = self.pad_unit.len();
        let whole = overflow.len() - overflow.len() % unit;
        let trailing_is_pad_prefix = self.pad_unit.starts_with(&overflow[whole..]);
        if !overflow[..whole]
            .chunks(unit)
            .all(|unit| unit == self.pad_unit)
            || !trailing_is_pad_prefix
        {
            return Err(ParamBuildError::StringTruncation);
        }
        Ok((&chunk[..split], consumed))
    }
}

/// The `ColumnSize` bound a buffered parameter is held to as its chunks arrive,
/// or `None` when the bound cannot be expressed in buffer bytes and is left to
/// the close-time conversion.
pub(crate) fn dae_length_limit(
    c_type: SqlSmallInt,
    sql_type: SqlSmallInt,
    column_size: usize,
) -> Result<Option<DaeLengthLimit>, ParamBuildError> {
    /// UTF-16LE blank, the pad unit for a `SQL_C_WCHAR` buffer.
    const BLANK_UTF16: &[u8] = &[b' ', 0];

    let (unit_bytes, pad_unit) = match c_type {
        SQL_C_WCHAR => (size_of::<u16>(), BLANK_UTF16),
        SQL_C_CHAR => (1, b" ".as_slice()),
        SQL_C_BINARY => (1, [0u8].as_slice()),
        other => return Err(ParamBuildError::UnsupportedCType(other)),
    };

    // A pairing can be measured here when the declaration's unit and the
    // buffer's unit are the same thing. Within the character family they always
    // are, whatever the wideness: `convert_character_sql` derives its limit from
    // `sql_type` and `ColumnSize` alone and hands it to `trim_blank_overflow`,
    // which measures the *source* in UTF-16 units -- the C type never enters
    // into the unit. So `varchar(n)` and `nvarchar(n)` both bound n UTF-16 units
    // of whatever buffer was bound, and a wideness mismatch is measurable on
    // exactly the same terms as a matched pair.
    //
    // Cross-*family* is the case that genuinely does not correspond: a binary
    // byte is not a character, so its bound is left to the close-time
    // conversion.
    //
    // The unit is an approximation of collation bytes on both paths -- msodbcsql
    // converts to the server code page to measure exactly
    // (`ValidatePutDataLength`, `odbc/sqlccmd.cpp:10931`), and closing that gap
    // is AB#47584. Agreeing with the materialized path is what matters here.
    let same_unit = match c_type {
        SQL_C_BINARY => sql_family(sql_type) == Some(SqlFamily::Binary),
        SQL_C_CHAR | SQL_C_WCHAR => sql_family(sql_type) == Some(SqlFamily::Character),
        _ => false,
    };
    if !same_unit {
        return Ok(None);
    }

    // The ceiling each declaration imposes, in its own unit. `char`/`binary`
    // reject a zero `ColumnSize` where the variable-width types read it as the
    // `max` spelling, so both go through the same helpers the materialized path
    // uses rather than a second reading of the rules.
    let units = match sql_type {
        SQL_CHAR => Some(usize::from(fixed_length(
            column_size,
            SQL_PREC_BIGCHARBINARY,
        )?)),
        SQL_WCHAR => Some(usize::from(fixed_length(column_size, SQL_PREC_NCHAR)?)),
        SQL_BINARY => Some(usize::from(fixed_length(
            column_size,
            SQL_PREC_BIGCHARBINARY,
        )?)),
        SQL_VARCHAR => variable_length(column_size, SQL_PREC_BIGCHARBINARY).map(usize::from),
        SQL_WVARCHAR => variable_length(column_size, SQL_PREC_NCHAR).map(usize::from),
        SQL_VARBINARY => variable_length(column_size, SQL_PREC_BIGCHARBINARY).map(usize::from),
        // `text`/`ntext`/`image` carry no declared length but are still bounded
        // by `ColumnSize`, as they are on the materialized path.
        SQL_LONGVARCHAR => Some(column_size.min(SQL_PREC_TEXTIMAGE)),
        SQL_WLONGVARCHAR => Some(column_size.min(SQL_PREC_NTEXT)),
        SQL_LONGVARBINARY => Some(column_size.min(SQL_PREC_TEXTIMAGE)),
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    };

    Ok(units.map(|units| DaeLengthLimit {
        // `SQL_C_CHAR` is the one buffer whose bytes are not its units: it holds
        // UTF-8, so the declaration's character count is measured in UTF-16
        // units to agree with the materialized path. The other two have a fixed
        // byte width per unit and stay exact.
        bound: match c_type {
            SQL_C_CHAR => DaeBound::Utf16Units(units),
            _ => DaeBound::Bytes(units.saturating_mul(unit_bytes)),
        },
        pad_unit,
    }))
}

/// Builds the RPC parameter for a data-at-execution value that was buffered
/// rather than streamed, from the bytes `SQLPutData` collected.
///
/// Routed through [`bound_param_to_rpc`] over a binding that points at the
/// collected buffer, so a streamed value is declared and converted by exactly
/// the code a materialized one is: `ParameterType` picks the declaration,
/// `ColumnSize` bounds it with the same trim-or-`22001` rule, and a cross-family
/// pairing transcodes instead of being refused (AB#47590).
///
/// `is_null` carries a parameter that `SQLPutData` marked `SQL_NULL_DATA`, which
/// must produce a typed NULL rather than an empty value.
pub(crate) fn buffered_dae_to_rpc(
    name: String,
    binding: &BoundParam,
    buffer: &[u8],
    is_null: bool,
) -> Result<RpcParameter, ParamBuildError> {
    let mut indicator: SqlLen = if is_null {
        SQL_NULL_DATA
    } else {
        SqlLen::try_from(buffer.len()).map_err(|_| ParamBuildError::StringTruncation)?
    };
    let mut synthetic = *binding;
    // Points at the collected bytes for the duration of this call only, which
    // is where the ODBC contract the materialized path relies on is met: the
    // buffer outlives the conversion and is not aliased while it runs.
    //
    // Both indicator pointers are repointed. `read_indicator` takes NULL from
    // `strlen_or_ind_ptr` and the *length* from `octet_length_ptr`, so leaving
    // the latter aimed at the application's buffer would hand the conversion
    // the `SQL_DATA_AT_EXEC` marker that started this sequence and it would
    // reject the value as unstaged.
    synthetic.parameter_value_ptr = buffer.as_ptr().cast_mut().cast();
    synthetic.buffer_length = indicator.max(0);
    synthetic.strlen_or_ind_ptr = &raw mut indicator;
    synthetic.octet_length_ptr = &raw mut indicator;
    unsafe { bound_param_to_rpc(name, &synthetic) }
}

/// How a data-at-execution parameter reaches the wire.
///
/// PLP framing is what makes streaming possible: the value body is an
/// unknown-length opener followed by length-prefixed chunks, so the parameter
/// header can go out before the total length is known. The types that can be
/// framed that way are exactly msodbcsql's `IsPartialLenType`
/// (`odbc/sqlcprot.h:1421`), and it keys the same decision on the *SQL type
/// alone* - `ColumnSize` does not enter into it, because a bounded declaration
/// is carried in the `@params` string while the value body stays a `max`
/// (AB#47590).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaePlan {
    /// Streamed as PLP chunks, each transcoded on the way out.
    Stream(StreamedSqlType),
    /// Not PLP-framable: collect the chunks and convert the value whole, as
    /// msodbcsql's `WriteToExtBuffer` branch does for a fixed-length or
    /// small-maximum target (`odbc/sqlccmd.cpp:4913`).
    Buffer,
}

impl DaePlan {
    pub(crate) fn is_buffered(self) -> bool {
        matches!(self, Self::Buffer)
    }
}

/// Chooses how a data-at-execution parameter reaches the wire.
///
/// Mirrors msodbcsql's `PutParamData` (`odbc/sqlccmd.cpp:4385`): a partial-length
/// SQL type streams its chunks, and everything else is cached until the value is
/// complete. The encoding difference between the buffer and the wire is handled
/// by [`DaeTranscode`] rather than by refusing to stream.
pub(crate) fn dae_plan(
    c_type: SqlSmallInt,
    sql_type: SqlSmallInt,
) -> Result<DaePlan, ParamBuildError> {
    if !matches!(c_type, SQL_C_CHAR | SQL_C_WCHAR | SQL_C_BINARY) {
        return Err(ParamBuildError::UnsupportedCType(c_type));
    }
    sql_family(sql_type).ok_or(ParamBuildError::UnsupportedSqlType(sql_type))?;

    Ok(match sql_type {
        SQL_VARCHAR | SQL_LONGVARCHAR => DaePlan::Stream(StreamedSqlType::VarcharMax),
        SQL_WVARCHAR | SQL_WLONGVARCHAR => DaePlan::Stream(StreamedSqlType::NVarcharMax),
        SQL_VARBINARY | SQL_LONGVARBINARY => DaePlan::Stream(StreamedSqlType::VarBinaryMax),
        _ => DaePlan::Buffer,
    })
}

/// The `@params` declaration a streamed parameter is given, or `None` when
/// `ColumnSize` names the `max` spelling and the declaration the streamed type
/// already carries is right.
///
/// The value body stays PLP-framed either way: only the variable it is assigned
/// to narrows. `text`/`ntext`/`image` keep their `max` substitution for the same
/// reason the materialized path does (AB#47592).
pub(crate) fn dae_streamed_declaration(
    sql_type: SqlSmallInt,
    column_size: usize,
) -> Result<Option<SqlType>, ParamBuildError> {
    Ok(match sql_type {
        SQL_VARCHAR => variable_length(column_size, SQL_PREC_BIGCHARBINARY)
            .map(|length| SqlType::Varchar(None, length)),
        SQL_WVARCHAR => variable_length(column_size, SQL_PREC_NCHAR)
            .map(|length| SqlType::NVarchar(None, length)),
        SQL_VARBINARY => variable_length(column_size, SQL_PREC_BIGCHARBINARY)
            .map(|length| SqlType::VarBinary(None, length)),
        SQL_LONGVARCHAR | SQL_WLONGVARCHAR | SQL_LONGVARBINARY => None,
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    })
}

/// The encoding of the application buffer feeding a streamed parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaeSource {
    /// Bytes with no character structure.
    Raw,
    /// `SQL_C_CHAR`, read as UTF-8 (see [`AppText`]).
    Utf8,
    /// `SQL_C_WCHAR`, UTF-16LE.
    Utf16,
}

/// The encoding the streamed chunks have to arrive in on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaeTarget {
    /// `varbinary(max)`: no re-encoding.
    Raw,
    /// `nvarchar(max)`: UTF-16LE, fixed by the type.
    Utf16,
    /// `varchar(max)`: the database collation's code page, applied by
    /// [`encode_narrow`] so an unmappable LCID falls back the same way the
    /// materialized path's does.
    Narrow(SqlCollation),
}

/// Converts one streamed chunk from the application's encoding into the wire's.
///
/// `write_streamed_chunk` writes bytes verbatim, so a narrow target's code page
/// has to be applied here - streaming a `SQL_C_CHAR` buffer straight into a
/// single-byte collation is what made `caf\u{e9}` arrive as two characters. This
/// is the same job msodbcsql gives `ConvertLongData` (`odbc/sqlccnvt.cpp:841`),
/// which likewise transcodes each chunk before pushing it (AB#47590).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DaeTranscode {
    source: DaeSource,
    target: DaeTarget,
}

impl DaeTranscode {
    /// The conversion a streamed parameter needs, given the connection's
    /// collation. A UTF-8 collation needs no re-encoding for a narrow target
    /// beyond the decode the source already implies.
    pub(crate) fn new(c_type: SqlSmallInt, sql_type: SqlSmallInt, collation: SqlCollation) -> Self {
        let source = match c_type {
            SQL_C_WCHAR => DaeSource::Utf16,
            SQL_C_CHAR => DaeSource::Utf8,
            _ => DaeSource::Raw,
        };
        let target = match sql_family(sql_type) {
            Some(SqlFamily::Character) if is_wide_character_sql_type(sql_type) => DaeTarget::Utf16,
            Some(SqlFamily::Character) => DaeTarget::Narrow(collation),
            _ => DaeTarget::Raw,
        };
        Self { source, target }
    }

    /// `true` when the buffer's bytes are already the wire's bytes, so a chunk
    /// can be written without being copied or carried.
    pub(crate) fn is_passthrough(&self) -> bool {
        match (self.source, self.target) {
            (DaeSource::Raw, _) => true,
            (DaeSource::Utf16, DaeTarget::Utf16) => true,
            // A UTF-8 collation wants exactly the bytes `SQL_C_CHAR` already
            // holds.
            (DaeSource::Utf8, DaeTarget::Narrow(collation)) => collation.utf8(),
            _ => false,
        }
    }

    /// Converts everything in `chunk` that completes a character, moving a
    /// trailing partial one into `carry` for the next call.
    pub(crate) fn push(&self, carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<u8> {
        if self.is_passthrough() {
            return chunk.to_vec();
        }
        let mut buf = std::mem::take(carry);
        buf.extend_from_slice(chunk);
        let split = buf.len() - self.incomplete_tail(&buf);
        carry.extend_from_slice(&buf[split..]);
        self.encode(&self.decode(&buf[..split]))
    }

    /// Converts what a value ended part-way through. The bytes cannot be
    /// completed, so they decode lossily, as the materialized path would.
    pub(crate) fn finish(&self, carry: &mut Vec<u8>) -> Vec<u8> {
        if carry.is_empty() {
            return Vec::new();
        }
        let tail = std::mem::take(carry);
        self.encode(&self.decode(&tail))
    }

    /// How many trailing bytes begin a character that is not finished yet.
    fn incomplete_tail(&self, buf: &[u8]) -> usize {
        match self.source {
            DaeSource::Raw => 0,
            DaeSource::Utf8 => incomplete_utf8_tail(buf),
            DaeSource::Utf16 => incomplete_utf16_tail(buf),
        }
    }

    fn decode(&self, bytes: &[u8]) -> String {
        match self.source {
            DaeSource::Utf16 => decode_utf16le(bytes),
            _ => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    fn encode(&self, text: &str) -> Vec<u8> {
        match self.target {
            DaeTarget::Utf16 => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            // The same helper the materialized narrow path uses, so an LCID
            // this crate cannot map falls back identically on both.
            DaeTarget::Narrow(collation) => encode_narrow(text, collation),
            DaeTarget::Raw => text.as_bytes().to_vec(),
        }
    }
}

/// Length of the trailing bytes of `buf` that begin a UTF-8 sequence which is
/// not finished yet. A sequence is at most four bytes, so only the last three
/// can be partial.
fn incomplete_utf8_tail(buf: &[u8]) -> usize {
    for back in 1..=3.min(buf.len()) {
        let byte = buf[buf.len() - back];
        if byte < 0x80 {
            // ASCII: a complete sequence on its own.
            return 0;
        }
        if byte >= 0xC0 {
            // A lead byte: complete only once all of its continuations landed.
            let needed = if byte >= 0xF0 {
                4
            } else if byte >= 0xE0 {
                3
            } else {
                2
            };
            return if back < needed { back } else { 0 };
        }
        // A continuation byte; keep walking back for the lead.
    }
    0
}

/// Length of the trailing bytes of `buf` that begin a UTF-16LE character which
/// is not finished yet: half a code unit, and a high surrogate still waiting for
/// its low half.
fn incomplete_utf16_tail(buf: &[u8]) -> usize {
    let odd = buf.len() % 2;
    let units_end = buf.len() - odd;
    if units_end >= 2 {
        let last = u16::from_le_bytes([buf[units_end - 2], buf[units_end - 1]]);
        if (0xD800..0xDC00).contains(&last) {
            return odd + 2;
        }
    }
    odd
}

/// The SQL-side axis of the conversion matrix.
///
/// [`AppValue`] is the C buffer normalised per family; `SqlFamily` picks the
/// converter that owns the target rules. A cross-family pairing is an adapter
/// between those canonical values - never a new per-pair conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlFamily {
    Integer,
    Character,
    Binary,
}

/// `None` for a SQL type no builder covers yet, which the bind-time matrix has
/// already rejected.
fn sql_family(sql_type: SqlSmallInt) -> Option<SqlFamily> {
    match sql_type {
        SQL_TINYINT | SQL_SMALLINT | SQL_INTEGER | SQL_BIGINT => Some(SqlFamily::Integer),
        SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR | SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR => {
            Some(SqlFamily::Character)
        }
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => Some(SqlFamily::Binary),
        _ => None,
    }
}

/// Application text in the encoding its C type dictates.
///
/// The narrow buffer is taken as UTF-8 because the only supported consumer is
/// mssql-python, which is UTF-8 native. msodbcsql instead reads it in the client
/// code page (`GetACP()` on Windows, `nl_langinfo(CODESET)` elsewhere -
/// `odbc/sqlcprot.h:2830`, `Common/include/Localization.hpp:742`), so the two
/// agree on a UTF-8 locale and differ on a default Windows one. The ODBC spec
/// fixes no encoding for `SQL_C_CHAR`. Client code page support is AB#47565.
enum AppText {
    Utf8(Vec<u8>),
    /// UTF-16LE bytes, not code units.
    Utf16(Vec<u8>),
}

impl AppText {
    /// Re-encodes into the target family's encoding.
    fn transcode(self, wide_target: bool) -> Self {
        match (self, wide_target) {
            (Self::Utf16(bytes), true) => Self::Utf16(bytes),
            (Self::Utf8(bytes), false) => {
                // Not a no-op: `SqlString`'s UTF-8 decode unwraps, so only hand
                // it checked bytes. Removable once that decode stops panicking
                // (AB#47576). Valid input keeps its allocation to the wire.
                match String::from_utf8(bytes) {
                    Ok(text) => Self::Utf8(text.into_bytes()),
                    Err(e) => Self::Utf8(
                        String::from_utf8_lossy(e.as_bytes())
                            .into_owned()
                            .into_bytes(),
                    ),
                }
            }
            (Self::Utf8(bytes), true) => Self::Utf16(
                String::from_utf8_lossy(&bytes)
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ),
            (Self::Utf16(bytes), false) => Self::Utf8(decode_utf16le(&bytes).into_bytes()),
        }
    }

    fn into_sql_string(self) -> SqlString {
        match self {
            Self::Utf8(bytes) => SqlString::new(bytes, EncodingType::Utf8),
            Self::Utf16(bytes) => SqlString::new(bytes, EncodingType::Utf16),
        }
    }

    /// Length in the units the declared length is compared against: UTF-16
    /// units, whatever the source encoding and whatever family it lands in.
    ///
    /// msodbcsql measures a wide source that way even for a narrow target
    /// (`sqlcfunc.cpp:2946`, "Assumption: 1 WCHAR converts to 1 byte") rather
    /// than encoding to find out. Applying the same unit to a narrow source is
    /// what keeps the two C types agreeing on one value - counting its UTF-8
    /// bytes instead would reject `caf\u{e9}` from a `varchar(4)` that
    /// `SQL_C_WCHAR` is allowed to fill.
    fn len_in_utf16_units(&self) -> usize {
        match self {
            Self::Utf16(bytes) => bytes.len() / size_of::<u16>(),
            // `from_utf8_lossy` borrows on the valid path, so this allocates
            // only to repair malformed input.
            Self::Utf8(bytes) => String::from_utf8_lossy(bytes).encode_utf16().count(),
        }
    }
}

/// Lossy UTF-16LE decode. An odd trailing byte is dropped: `read_wchar_bytes`
/// already floors the length to whole units, so this only guards a caller that
/// bypasses it.
fn decode_utf16le(bytes: &[u8]) -> String {
    char::decode_utf16(
        bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
    )
    .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
    .collect()
}

/// Emits the character `SqlType` named by `ParameterType`, transcoding when the
/// C type and the target are of different families.
///
/// `ColumnSize` is a character count for every character SQL type, per the ODBC
/// "Column Size" appendix.
fn convert_character_sql(
    sql_type: SqlSmallInt,
    column_size: usize,
    text: AppText,
) -> Result<SqlType, ParamBuildError> {
    let wide = is_wide_character_sql_type(sql_type);
    let max_length = if wide {
        SQL_PREC_NCHAR
    } else {
        SQL_PREC_BIGCHARBINARY
    };

    // The declared length for the T-SQL type, and the bound truncation is
    // measured against. They differ only for `text`/`ntext`, which carry no
    // declared length but are still bounded by `ColumnSize`
    // (`sqlcfunc.cpp:2898`); only the `max` types are unbounded.
    let (declared, limit) = match sql_type {
        SQL_CHAR | SQL_WCHAR => {
            let length = fixed_length(column_size, max_length)?;
            (Some(length), Some(usize::from(length)))
        }
        SQL_VARCHAR | SQL_WVARCHAR => {
            let declared = variable_length(column_size, max_length);
            (declared, declared.map(usize::from))
        }
        SQL_LONGVARCHAR => (None, Some(column_size.min(SQL_PREC_TEXTIMAGE))),
        SQL_WLONGVARCHAR => (None, Some(column_size.min(SQL_PREC_NTEXT))),
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    };

    // Fit before transcoding: the limit counts source units, not the encoded
    // result - see [`trim_blank_overflow`].
    let text = match limit {
        Some(limit) => trim_blank_overflow(text, limit)?,
        None => text,
    };
    let value = Some(text.transcode(wide).into_sql_string());

    Ok(match (sql_type, declared) {
        (SQL_CHAR, Some(length)) => SqlType::Char(value, length),
        (SQL_WCHAR, Some(length)) => SqlType::NChar(value, length),
        (SQL_VARCHAR, Some(length)) => SqlType::Varchar(value, length),
        (SQL_VARCHAR, None) => SqlType::VarcharMax(value),
        (SQL_WVARCHAR, Some(length)) => SqlType::NVarchar(value, length),
        (SQL_WVARCHAR, None) => SqlType::NVarcharMax(value),
        // `text` / `ntext` would be the msodbcsql default, but `mssql-tds`
        // serializes them in bulk-copy ROW format and the server rejects the
        // RPC (AB#47591). `max` is what msodbcsql itself sends under
        // `SQL_COPT_SS_LONGASMAX`, and the `ColumnSize` bound above still
        // applies, so only the declaration differs. Restoring it is AB#47592.
        (SQL_LONGVARCHAR, _) => SqlType::VarcharMax(value),
        (SQL_WLONGVARCHAR, _) => SqlType::NVarcharMax(value),
        // Unreachable: the match above already rejected everything else.
        (other, _) => return Err(ParamBuildError::UnsupportedSqlType(other)),
    })
}

/// Trims `text` to `limit` units, or reports `22001`.
///
/// The unit is an approximation on purpose: `varchar(n)` bounds *collation*
/// bytes and the collation is only applied downstream by `serialize_string`, so
/// the exact count is unknowable here. Every source is measured in UTF-16 units
/// instead - msodbcsql's own unit in three of its four arms
/// (`sqlcfunc.cpp:2946`, `:2935`). Holding the fourth to it too is a registered
/// deviation; see `.github/instructions/mssql-odbc.instructions.md`.
///
/// Overflowing *blanks* are dropped silently, anything else is `22001` -
/// msodbcsql checks the overflow with `CheckTrailingChars` first
/// (`sqlcfunc.cpp:2957`), and inbound truncation is an error unlike the benign
/// outbound `01004`.
///
/// TODO: the count errs low, so an over-long value can still reach the wire.
/// `serialize_string` can grow it - GB18030 emits 4 bytes where UTF-8 uses 2 -
/// and `serialize_char_varchar_direct` then reports an opaque `UsageError`
/// rather than `22001`, the `max` and `text`/`ntext` types not even that.
/// Exactness needs the collation at this layer (AB#47584).
///
/// TODO: msodbcsql's narrow-to-wide arm never reaches this logic - its walk
/// tests `cchDest > cchMax` before incrementing and `break`s past the trim
/// (`sqlcfunc.cpp:2926`), so one character of overflow escapes and overflowing
/// blanks survive. Deliberately not replicated.
fn trim_blank_overflow(text: AppText, limit: usize) -> Result<AppText, ParamBuildError> {
    const BLANK_UTF16: [u8; 2] = [b' ', 0];

    // A UTF-8 buffer never yields more UTF-16 units than it has bytes, so fitting
    // by byte count settles it without the walk - msodbcsql short-circuits the
    // same way at `sqlcfunc.cpp:2917`.
    if let AppText::Utf8(bytes) = &text
        && bytes.len() <= limit
    {
        return Ok(text);
    }

    let overflow = text.len_in_utf16_units().saturating_sub(limit);
    if overflow == 0 {
        return Ok(text);
    }

    // Only trailing blanks may be dropped, and a blank is one unit in both
    // encodings, so the overflow maps 1:1 onto trailing source units. That also
    // makes `overflow` usable as a byte count on the UTF-8 arm below: `b' '`
    // never occurs as a continuation byte, so an all-blank tail is that many
    // whole characters and the cut lands on a boundary.
    Ok(match text {
        AppText::Utf8(mut bytes) => {
            let keep = bytes.len().saturating_sub(overflow);
            if bytes[keep..].iter().any(|b| *b != b' ') {
                return Err(ParamBuildError::StringTruncation);
            }
            bytes.truncate(keep);
            AppText::Utf8(bytes)
        }
        AppText::Utf16(mut bytes) => {
            let keep = bytes.len().saturating_sub(overflow * size_of::<u16>());
            if bytes[keep..]
                .chunks_exact(size_of::<u16>())
                .any(|unit| unit != BLANK_UTF16)
            {
                return Err(ParamBuildError::StringTruncation);
            }
            bytes.truncate(keep);
            AppText::Utf16(bytes)
        }
    })
}

/// Emits the binary `SqlType` named by `ParameterType` and `ColumnSize`.
///
/// `ColumnSize` is a byte count here, where the character path counts characters.
fn convert_binary_sql(
    sql_type: SqlSmallInt,
    column_size: usize,
    bytes: Vec<u8>,
) -> Result<SqlType, ParamBuildError> {
    // `image` carries no declared length but is still bounded by `ColumnSize`,
    // exactly as `text`/`ntext` are; only `varbinary(max)` is unbounded.
    let (declared, limit) = match sql_type {
        SQL_BINARY => {
            let length = fixed_length(column_size, SQL_PREC_BIGCHARBINARY)?;
            (Some(length), Some(usize::from(length)))
        }
        SQL_VARBINARY => {
            let declared = variable_length(column_size, SQL_PREC_BIGCHARBINARY);
            (declared, declared.map(usize::from))
        }
        SQL_LONGVARBINARY => (None, Some(column_size.min(SQL_PREC_TEXTIMAGE))),
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    };

    let bytes = match limit {
        Some(limit) => trim_zero_overflow(bytes, limit)?,
        None => bytes,
    };

    Ok(match (sql_type, declared) {
        (SQL_BINARY, Some(length)) => SqlType::Binary(Some(bytes), length),
        (SQL_VARBINARY, Some(length)) => SqlType::VarBinary(Some(bytes), length),
        (SQL_VARBINARY, None) => SqlType::VarBinaryMax(Some(bytes)),
        // `image` would be the msodbcsql default; `mssql-tds` serializes it in
        // bulk-copy ROW format and the server rejects the RPC, so `max` goes out
        // instead - the same substitution `text`/`ntext` carry (AB#47592). The
        // `ColumnSize` bound above still applies, so only the declaration differs.
        (SQL_LONGVARBINARY, _) => SqlType::VarBinaryMax(Some(bytes)),
        // Unreachable: the match above already rejected everything else.
        (other, _) => return Err(ParamBuildError::UnsupportedSqlType(other)),
    })
}

/// Trims `bytes` to `limit` when the overflow is entirely *zeros*, or reports
/// `22001`.
///
/// The character sibling is [`trim_blank_overflow`]; this is
/// `CheckTrailingZeros` (`sqlccnvt.cpp:8690`) driving the binary arm at
/// `sqlcfunc.cpp:2611`, where that one is `CheckTrailingChars` with a blank.
/// A partially zero overflow is `22001` in both: the scan stops at the first
/// byte that is not padding, wherever it sits.
///
/// Unlike the character sibling this count is **exact** - `ColumnSize` bounds
/// bytes and bytes are what reach the wire, so there is no collation to
/// approximate and no registered deviation to carry.
fn trim_zero_overflow(mut bytes: Vec<u8>, limit: usize) -> Result<Vec<u8>, ParamBuildError> {
    if bytes.len() <= limit {
        return Ok(bytes);
    }
    if bytes[limit..].iter().any(|b| *b != 0) {
        return Err(ParamBuildError::StringTruncation);
    }
    bytes.truncate(limit);
    Ok(bytes)
}

/// Emits the integer `SqlType` named by `ParameterType`, not by the C type, so
/// `@P1` is declared as the application asked. A value outside the target's
/// range is `22003` rather than a silently wrapped wire value.
fn convert_integer_sql(sql_type: SqlSmallInt, v: i128) -> Result<SqlType, ParamBuildError> {
    Ok(match sql_type {
        SQL_TINYINT => {
            SqlType::TinyInt(Some(narrow_i128::<u8>(v).map_err(ParamBuildError::Value)?))
        }
        SQL_SMALLINT => {
            SqlType::SmallInt(Some(narrow_i128::<i16>(v).map_err(ParamBuildError::Value)?))
        }
        SQL_INTEGER => SqlType::Int(Some(narrow_i128::<i32>(v).map_err(ParamBuildError::Value)?)),
        SQL_BIGINT => SqlType::BigInt(Some(narrow_i128::<i64>(v).map_err(ParamBuildError::Value)?)),
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    })
}

/// Formats an integer source for the character converter.
///
/// `read_param_value` applied each C type's own signedness while widening, so
/// there is no per-C-type arm here. Radix 10 matches `ConvertToChar`
/// (`sqlccnvt.cpp:1634`).
fn integer_as_text(v: i128) -> AppText {
    AppText::Utf8(v.to_string().into_bytes())
}

/// Parses a character source for the integer converter.
///
/// Neither end is a float type - the fraction lives in the text, as in `"12.7"`
/// into an `int`. Two rules, both msodbcsql's:
///
/// - Discarding a non-zero fraction is an error (`22001`) here, where fetch only
///   warns (`01S07`): `ParamToSQLType` rewrites it for a non-2.x application
///   (`sqlcfunc.cpp:3348`).
/// - Overflow outranks it, so a value doing both reports `22003` - msodbcsql
///   narrows before that rewrite runs, which is why the order below is narrow,
///   then check the fraction.
fn integer_from_text(sql_type: SqlSmallInt, text: AppText) -> Result<SqlType, ParamBuildError> {
    // Borrowed so a valid narrow buffer - the ordinary case - is parsed in
    // place; `from_utf8_lossy` allocates only to repair malformed input.
    let decoded: Cow<'_, str> = match &text {
        AppText::Utf8(bytes) => String::from_utf8_lossy(bytes),
        AppText::Utf16(bytes) => Cow::Owned(decode_utf16le(bytes)),
    };
    let (v, fraction_dropped) = parse_numeric_text(&decoded)
        .map_err(ParamBuildError::Value)?
        .to_i128_truncating()
        .ok_or(ParamBuildError::Value(ConvError::OutOfRange))?;
    let value = convert_integer_sql(sql_type, v)?;
    if fraction_dropped {
        return Err(ParamBuildError::StringTruncation);
    }
    Ok(value)
}

/// A TDS value plus the precision/scale the RPC layer must use for both the
/// `@P1 <type>` declaration and the wire `TYPE_INFO`.
type TypedValue = (SqlType, Option<RpcTypeMetadata>);

/// Largest fractional-seconds scale of `time`/`datetime2`/`datetimeoffset`.
const MAX_DATETIME_SCALE: u8 = 7;

/// Typed NULL for a bound parameter.
///
/// The type comes from `sql_type` whether or not the binding was defaulted: the
/// C type only says how the value buffer would have been read, and a NULL has no
/// buffer to read. msodbcsql does the same - `CRPCSQLSender::SendParamList`
/// (`Sql/Ntdbms/sqlncli/odbc/sqlccmd.cpp`) builds the `@Pn <type>` declaration
/// from `lpparam->fSqlType` and never consults the bound C type.
///
/// `column_size` and `decimal_digits` come straight from the application, so
/// every value that participates in the `@P1 <type>` declaration is validated
/// here: emitting `decimal(0,0)` or `char(0)` would otherwise fail server-side
/// with an opaque syntax error instead of `HY104` at execute time.
///
/// The returned [`RpcTypeMetadata`] is the *only* place precision and scale are
/// carried. [`RpcParameter`] uses it to render the declaration and to write the
/// wire `TYPE_INFO`, so the two cannot drift apart.
fn typed_null(
    sql_type: SqlSmallInt,
    column_size: usize,
    decimal_digits: SqlSmallInt,
) -> Result<TypedValue, ParamBuildError> {
    let value = match sql_type {
        SQL_BIT => SqlType::Bit(None),
        SQL_TINYINT => SqlType::TinyInt(None),
        SQL_SMALLINT => SqlType::SmallInt(None),
        SQL_INTEGER => SqlType::Int(None),
        SQL_BIGINT => SqlType::BigInt(None),
        SQL_REAL => SqlType::Real(None),
        SQL_FLOAT | SQL_DOUBLE => SqlType::Float(None),
        SQL_DECIMAL => {
            let metadata = decimal_metadata(column_size, decimal_digits)?;
            return Ok((SqlType::Decimal(None), Some(metadata)));
        }
        SQL_NUMERIC => {
            let metadata = decimal_metadata(column_size, decimal_digits)?;
            return Ok((SqlType::Numeric(None), Some(metadata)));
        }
        SQL_CHAR => SqlType::Char(None, fixed_length(column_size, SQL_PREC_BIGCHARBINARY)?),
        SQL_VARCHAR => match variable_length(column_size, SQL_PREC_BIGCHARBINARY) {
            Some(length) => SqlType::Varchar(None, length),
            None => SqlType::VarcharMax(None),
        },
        // TODO: AB#47592
        SQL_LONGVARCHAR => SqlType::VarcharMax(None),
        SQL_WCHAR => SqlType::NChar(None, fixed_length(column_size, SQL_PREC_NCHAR)?),
        SQL_WVARCHAR => match variable_length(column_size, SQL_PREC_NCHAR) {
            Some(length) => SqlType::NVarchar(None, length),
            None => SqlType::NVarcharMax(None),
        },
        // TODO: AB#47592
        SQL_WLONGVARCHAR => SqlType::NVarcharMax(None),
        SQL_BINARY => SqlType::Binary(None, fixed_length(column_size, SQL_PREC_BIGCHARBINARY)?),
        SQL_VARBINARY => match variable_length(column_size, SQL_PREC_BIGCHARBINARY) {
            Some(length) => SqlType::VarBinary(None, length),
            None => SqlType::VarBinaryMax(None),
        },
        SQL_LONGVARBINARY => SqlType::VarBinaryMax(None),
        SQL_GUID => SqlType::Uuid(None),
        SQL_TYPE_DATE => SqlType::Date(None),
        SQL_TYPE_TIME | SQL_SS_TIME2 => {
            let metadata = datetime_metadata(decimal_digits)?;
            return Ok((SqlType::Time(None), Some(metadata)));
        }
        SQL_TYPE_TIMESTAMP => {
            let metadata = datetime_metadata(decimal_digits)?;
            return Ok((SqlType::DateTime2(None), Some(metadata)));
        }
        SQL_SS_TIMESTAMPOFFSET => {
            let metadata = datetime_metadata(decimal_digits)?;
            return Ok((SqlType::DateTimeOffset(None), Some(metadata)));
        }
        SQL_SS_XML => SqlType::Xml(None),
        // A NULL `sql_variant` carries no payload, so the inner type only has to
        // be a legal one - it never reaches the wire.
        SQL_SS_VARIANT => SqlType::Variant(Box::new(SqlType::Varchar(None, 1))),
        SQL_SS_VECTOR => {
            let (dimensions, base_type) = vector_metadata(column_size, decimal_digits)?;
            SqlType::Vector(None, dimensions, base_type)
        }
        // `SQL_SS_UDT` and `SQL_SS_TABLE` need the fully qualified server type
        // name, which `SQLDescribeParam` does not report and this driver has no
        // other way to obtain, so they are rejected up front at bind time.
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    };
    Ok((value, None))
}

/// Length of a fixed-width `char`/`nchar`/`binary` declaration. Zero-length and
/// oversized declarations are invalid T-SQL and have no `max` spelling.
///
/// Matches msodbcsql for ODBC 3.x applications: `CheckSqlPrec`
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp`) rejects a zero `ColumnSize` on these
/// types with `HY104`, and only clamps it to the maximum for a 2.x application
/// (`IS2xAPP`). We report the same `HY104`, at execute rather than at bind.
/// `varchar`/`nvarchar` differ deliberately -- see [`variable_length`].
fn fixed_length(column_size: usize, max: usize) -> Result<u16, ParamBuildError> {
    u16::try_from(column_size)
        .ok()
        .filter(|_| (1..=max).contains(&column_size))
        .ok_or(ParamBuildError::InvalidParameterSize(column_size))
}

/// Length of a `varchar`/`nvarchar`/`varbinary` declaration, or `None` for the
/// `max` spelling.
///
/// `SQLDescribeParam` reports 0 for `*(max)` parameters, and an application may
/// legitimately pass a `ColumnSize` past the non-`max` limit; both widen to
/// `max` rather than erroring, matching `RpcParameter::get_sql_name`.
///
/// msodbcsql draws the same line at 0: against a Yukon-or-later server
/// `CheckSqlPrecScale` checks only the upper bound for these types
/// (`sqlcdesc.cpp:11722` for `SQL_WVARCHAR`, `:11748` for `SQL_VARCHAR`) and
/// skips the zero check the fixed-length types get, because 0 *is* the `max`
/// spelling. Past the bound it does report `HY104` - so does the bind gate,
/// which makes the widening here unreachable through the API.
fn variable_length(column_size: usize, max: usize) -> Option<u16> {
    if column_size == 0 || column_size > max {
        None
    } else {
        u16::try_from(column_size).ok()
    }
}

fn decimal_metadata(
    column_size: usize,
    decimal_digits: SqlSmallInt,
) -> Result<RpcTypeMetadata, ParamBuildError> {
    let precision = u8::try_from(column_size)
        .ok()
        .filter(|_| (1..=SQL_PREC_NUMERIC).contains(&column_size))
        .ok_or(ParamBuildError::InvalidParameterSize(column_size))?;
    let scale = u8::try_from(decimal_digits)
        .ok()
        .filter(|scale| *scale <= precision)
        .ok_or(ParamBuildError::InvalidDecimalDigits(decimal_digits))?;
    Ok(RpcTypeMetadata {
        precision: Some(precision),
        scale: Some(scale),
    })
}

fn datetime_metadata(decimal_digits: SqlSmallInt) -> Result<RpcTypeMetadata, ParamBuildError> {
    let scale = u8::try_from(decimal_digits)
        .ok()
        .filter(|scale| *scale <= MAX_DATETIME_SCALE)
        .ok_or(ParamBuildError::InvalidDecimalDigits(decimal_digits))?;
    Ok(RpcTypeMetadata {
        precision: None,
        scale: Some(scale),
    })
}

/// Recovers a vector's dimension count and base type from the `ColumnSize` and
/// `DecimalDigits` that `SQLDescribeParam` reported.
///
/// `ColumnSize` is the size of the whole client buffer - a
/// [`SqlSsVectorLayout`] header followed by `dimensions` elements. msodbcsql
/// always exchanges those elements as 4-byte floats regardless of the
/// server-side base type, so the element width is
/// [`SQL_SS_VECTOR_ELEMENT_SIZE`] and not the base type's own width.
/// `DecimalDigits` carries the base type (`0` = float32, `1` = float16),
/// mirroring `SQL_SS_VECTOR`'s `SQL_CA_SS_VECTOR_BASE_TYPE` descriptor field.
fn vector_metadata(
    column_size: usize,
    base_type: SqlSmallInt,
) -> Result<(u16, VectorBaseType), ParamBuildError> {
    let payload_size = column_size
        .checked_sub(std::mem::size_of::<SqlSsVectorLayout>())
        .filter(|size| size % SQL_SS_VECTOR_ELEMENT_SIZE == 0)
        .ok_or(ParamBuildError::InvalidParameterSize(column_size))?;
    let dimensions = u16::try_from(payload_size / SQL_SS_VECTOR_ELEMENT_SIZE)
        .map_err(|_| ParamBuildError::InvalidParameterSize(column_size))?;
    let base_type = match base_type {
        0 => VectorBaseType::Float32,
        1 => VectorBaseType::Float16,
        _ => return Err(ParamBuildError::InvalidDecimalDigits(base_type)),
    };
    Ok((dimensions, base_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_DEFAULT, SQL_C_FLOAT, SQL_C_LONG, SQL_C_SBIGINT, SQL_C_SLONG,
        SQL_C_STINYINT, SQL_C_TINYINT, SQL_C_UBIGINT, SQL_C_WCHAR, SQL_DATA_AT_EXEC,
        SQL_DEFAULT_PARAM, SQL_NO_TOTAL, SQL_NTS, SQL_NULL_DATA, SQL_PARAM_INPUT, SQL_SS_UDT,
        SqlULen,
    };
    use crate::params::conversion_matrix::is_supported_conversion;
    use std::ffi::c_void;

    /// A `ColumnSize` past every non-`max` limit, as an application binding an
    /// unbounded value may plausibly pass. Not an ODBC constant: msodbcsql's
    /// unbounded sentinel is `0`, so there is no header name for this value.
    const OVERSIZED_COLUMN_SIZE: SqlULen = 2_147_483_647;

    fn param(c_type: SqlSmallInt, ptr: *mut c_void, ind: *mut SqlLen) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type,
            sql_type: 0,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: ptr,
            buffer_length: 0,
            strlen_or_ind_ptr: ind,
            octet_length_ptr: ind,
        }
    }

    /// A parameter bound with `SQL_C_DEFAULT`, whose `c_type` `SQLBindParameter`
    /// has already resolved - nothing then distinguishes it from an explicit
    /// bind of that same C type.
    fn default_param(sql_type: SqlSmallInt, ind: *mut SqlLen) -> BoundParam {
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), ind);
        p.sql_type = sql_type;
        p
    }

    /// A `SQL_C_BINARY` binding. `ParameterType` is carried because a NULL is
    /// typed from it alone - the C type says only how a value buffer would have
    /// been read, and a NULL has no buffer.
    fn binary_param(ptr: *mut c_void, ind: *mut SqlLen) -> BoundParam {
        let mut p = param(SQL_C_BINARY, ptr, ind);
        p.sql_type = SQL_VARBINARY;
        p
    }

    fn int_param(
        c_type: SqlSmallInt,
        sql_type: SqlSmallInt,
        ptr: *mut c_void,
        ind: *mut SqlLen,
    ) -> BoundParam {
        let mut p = param(c_type, ptr, ind);
        p.sql_type = sql_type;
        p
    }

    /// One buffer, four declarations: the wire type follows `ParameterType`, so
    /// the server sees the type the application asked for rather than the width
    /// of the C buffer it happened to bind.
    #[test]
    fn parameter_type_names_the_wire_type_not_the_c_type() {
        let cases: &[(SqlSmallInt, SqlType)] = &[
            (SQL_TINYINT, SqlType::TinyInt(Some(7))),
            (SQL_SMALLINT, SqlType::SmallInt(Some(7))),
            (SQL_INTEGER, SqlType::Int(Some(7))),
            (SQL_BIGINT, SqlType::BigInt(Some(7))),
        ];
        for (sql_type, expected) in cases {
            let mut v: i32 = 7;
            let mut ind: SqlLen = 4;
            let p = int_param(
                SQL_C_SLONG,
                *sql_type,
                &mut v as *mut i32 as *mut c_void,
                &mut ind,
            );
            let (value, metadata) = unsafe { bound_param_to_value(&p) }.unwrap();
            assert_eq!(&value, expected, "case: sql_type {sql_type}");
            assert_eq!(metadata, None, "case: sql_type {sql_type}");
        }
    }

    /// A value the target cannot hold is 22003, not a silently wrapped value.
    #[test]
    fn value_outside_the_target_range_is_22003() {
        let mut v: i32 = 300;
        let mut ind: SqlLen = 4;
        let p = int_param(
            SQL_C_SLONG,
            SQL_TINYINT,
            &mut v as *mut i32 as *mut c_void,
            &mut ind,
        );
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::Value(ConvError::OutOfRange));
        assert_eq!(err.diag().state, *b"22003");
    }

    /// `SQL_C_UBIGINT` is the one C type whose range exceeds every SQL target.
    #[test]
    fn ubigint_above_i64_max_is_22003() {
        let mut v: u64 = i64::MAX as u64 + 1;
        let mut ind: SqlLen = 8;
        let p = int_param(
            SQL_C_UBIGINT,
            SQL_BIGINT,
            &mut v as *mut u64 as *mut c_void,
            &mut ind,
        );
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::Value(ConvError::OutOfRange));
    }

    #[test]
    fn integer_null_is_typed_by_parameter_type() {
        for c_type in [SQL_C_SLONG, SQL_C_UBIGINT, SQL_C_STINYINT] {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = int_param(c_type, SQL_INTEGER, std::ptr::null_mut(), &mut ind);
            p.column_size = 10;
            let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
            assert!(matches!(value, SqlType::Int(None)), "c_type {c_type}");
        }
    }

    /// Every SQL type bind time calls `Supported` must have a NULL spelling, so
    /// a parameter that binds can never fail at execute with "SQL type not
    /// implemented". `SQL_SS_UDT` / `SQL_SS_TABLE` are the documented exception:
    /// they need a server type name no describe call reports, and are rejected
    /// at bind instead. Metadata validity is a separate concern, so only the
    /// missing-arm error is checked.
    #[test]
    fn every_supported_sql_type_has_a_typed_null() {
        use crate::api::odbc_types::SQL_SS_TABLE;
        use crate::api::type_rules::{SqlTypeSupport, classify_parameter_sql_type};

        let mut checked = 0;
        for sql_type in -160..=120 {
            if classify_parameter_sql_type(sql_type) != SqlTypeSupport::Supported
                || matches!(sql_type, SQL_SS_UDT | SQL_SS_TABLE)
            {
                continue;
            }
            checked += 1;
            assert!(
                !matches!(
                    typed_null(sql_type, 10, 0),
                    Err(ParamBuildError::UnsupportedSqlType(_))
                ),
                "no typed NULL for supported SQL type {sql_type}"
            );
        }
        assert!(checked > 0, "no supported SQL types found");
    }

    /// The matrix promises at bind time that execute can convert the pairing, so
    /// no pairing it accepts may come back "not implemented" from here.
    #[test]
    fn every_accepted_pairing_is_convertible() {
        let mut buf = [0u8; 32];
        let mut checked = 0;
        for c_type in -160..=120 {
            // Resolved at bind, so it never reaches the matrix.
            if c_type == SQL_C_DEFAULT {
                continue;
            }
            for sql_type in -160..=120 {
                if !is_supported_conversion(c_type, sql_type) {
                    continue;
                }
                let mut ind: SqlLen = 0;
                let mut p = param(c_type, buf.as_mut_ptr() as *mut c_void, &mut ind);
                p.sql_type = sql_type;
                p.column_size = 10;
                if let Err(
                    e @ (ParamBuildError::UnsupportedCType(_)
                    | ParamBuildError::UnsupportedSqlType(_)
                    | ParamBuildError::ConversionNotImplemented),
                ) = unsafe { bound_param_to_value(&p) }
                {
                    panic!(
                        "matrix accepts {c_type} -> {sql_type} but the converter returned {e:?}"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 0, "matrix iteration found no supported pairings");
    }

    #[test]
    fn char_nts_becomes_varchar() {
        let mut buf: Vec<u8> = b"hello\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_VARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    /// Non-ASCII must survive the narrow path. `SqlString`'s UTF-8 decode
    /// carries a TODO claiming UTF-16 decode "works better"; this pins that the
    /// UTF-8 tag round-trips so the claim cannot silently become true.
    #[test]
    fn char_round_trips_non_ascii_utf8() {
        let mut buf: Vec<u8> = "caf\u{e9} \u{2615}\0".as_bytes().to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_VARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => {
                assert_eq!(s.to_utf8_string(), "caf\u{e9} \u{2615}")
            }
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn binary_with_indicator_length_becomes_varbinary() {
        let mut buf: Vec<u8> = vec![0x01, 0x00, 0xFF, 0x7E];
        let mut ind: SqlLen = 4;
        let p = binary_param(buf.as_mut_ptr() as *mut c_void, &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarBinaryMax(Some(b)) => assert_eq!(b, vec![0x01, 0x00, 0xFF, 0x7E]),
            other => panic!("expected VarBinaryMax(Some), got {other:?}"),
        }
    }

    /// Without an indicator pointer a binary buffer has no stated length, so
    /// the binding's `BufferLength` supplies it.
    #[test]
    fn binary_without_indicator_uses_buffer_length() {
        let mut buf: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let mut p = binary_param(buf.as_mut_ptr() as *mut c_void, std::ptr::null_mut());
        p.buffer_length = 2;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarBinaryMax(Some(b)) => assert_eq!(b, vec![0xDE, 0xAD]),
            other => panic!("expected VarBinaryMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn binary_null_indicator_becomes_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = binary_param(std::ptr::null_mut(), &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarBinaryMax(None)));
    }

    /// A zero-length binary value is empty, not NULL — `SQL_NULL_DATA` is the
    /// only way to bind a NULL.
    #[test]
    fn binary_zero_length_becomes_empty_varbinary() {
        let mut buf: Vec<u8> = vec![0xDE, 0xAD];
        let mut ind: SqlLen = 0;
        let p = binary_param(buf.as_mut_ptr() as *mut c_void, &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarBinaryMax(Some(b)) => assert!(b.is_empty()),
            other => panic!("expected VarBinaryMax(Some), got {other:?}"),
        }
    }

    /// `SQL_NTS` on a binary parameter falls back to `BufferLength`, which has
    /// no NUL to stop at and so must be a real length.
    #[test]
    fn binary_nts_with_negative_buffer_length_is_rejected() {
        let mut buf: Vec<u8> = vec![0x01, 0x02];
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let mut p = binary_param(buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.buffer_length = -1;
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert!(matches!(err, ParamBuildError::InvalidLength(-1)));
        assert_eq!(err.diag().state, ERR_INVALID_STRING_OR_BUFFER_LENGTH.state);
    }

    /// The three C types a value can be supplied in, and the rejection every
    /// other binding gets. `SQLBindParameter` accepts data-at-execution for any
    /// C type, so the refusal lands at execute time.
    #[test]
    fn dae_plan_covers_the_supported_c_types() {
        assert_eq!(
            dae_plan(SQL_C_CHAR, SQL_VARCHAR),
            Ok(DaePlan::Stream(StreamedSqlType::VarcharMax))
        );
        assert_eq!(
            dae_plan(SQL_C_WCHAR, SQL_WVARCHAR),
            Ok(DaePlan::Stream(StreamedSqlType::NVarcharMax))
        );
        assert_eq!(
            dae_plan(SQL_C_BINARY, SQL_VARBINARY),
            Ok(DaePlan::Stream(StreamedSqlType::VarBinaryMax))
        );

        let err = dae_plan(SQL_C_LONG, SQL_VARCHAR).unwrap_err();
        assert!(matches!(err, ParamBuildError::UnsupportedCType(SQL_C_LONG)));
        assert_eq!(err.diag().state, ERR_PARAM_C_TYPE_NOT_IMPLEMENTED.state);
    }

    /// The streamed set is the PLP-framable one, and it does not depend on
    /// `ColumnSize`: a bounded `varchar(10)` still streams its body and carries
    /// its length in the declaration instead. This is msodbcsql's
    /// `IsPartialLenType` (`odbc/sqlcprot.h:1421`) driving the same branch in
    /// `PutParamData` (AB#47590).
    #[test]
    fn dae_plan_streams_every_partial_length_type() {
        for (c_type, sql_type) in [
            (SQL_C_CHAR, SQL_VARCHAR),
            (SQL_C_CHAR, SQL_LONGVARCHAR),
            (SQL_C_WCHAR, SQL_WVARCHAR),
            (SQL_C_WCHAR, SQL_WLONGVARCHAR),
            (SQL_C_BINARY, SQL_VARBINARY),
            (SQL_C_BINARY, SQL_LONGVARBINARY),
        ] {
            assert!(
                matches!(dae_plan(c_type, sql_type), Ok(DaePlan::Stream(_))),
                "{c_type}/{sql_type} is PLP-framable and should stream"
            );
        }
    }

    /// Everything that cannot be PLP-framed is collected and converted whole,
    /// the branch msodbcsql serves with `WriteToExtBuffer`
    /// (`odbc/sqlccmd.cpp:4913`).
    #[test]
    fn dae_plan_buffers_what_cannot_be_plp_framed() {
        for (c_type, sql_type) in [
            (SQL_C_CHAR, SQL_CHAR),
            (SQL_C_WCHAR, SQL_WCHAR),
            (SQL_C_BINARY, SQL_BINARY),
            (SQL_C_CHAR, SQL_INTEGER),
            (SQL_C_WCHAR, SQL_BIGINT),
        ] {
            assert_eq!(
                dae_plan(c_type, sql_type),
                Ok(DaePlan::Buffer),
                "{c_type}/{sql_type} cannot be PLP-framed"
            );
        }
    }

    /// A bounded declaration narrows the `@params` entry while the value body
    /// stays PLP; `ColumnSize` 0 is the `max` spelling and needs none.
    #[test]
    fn dae_streamed_declaration_narrows_only_a_bounded_parameter() {
        assert_eq!(dae_streamed_declaration(SQL_VARCHAR, 0), Ok(None));
        assert_eq!(
            dae_streamed_declaration(SQL_VARCHAR, 10),
            Ok(Some(SqlType::Varchar(None, 10)))
        );
        assert_eq!(
            dae_streamed_declaration(SQL_WVARCHAR, 10),
            Ok(Some(SqlType::NVarchar(None, 10)))
        );
        assert_eq!(
            dae_streamed_declaration(SQL_VARBINARY, 10),
            Ok(Some(SqlType::VarBinary(None, 10)))
        );
        // `text`/`ntext`/`image` keep the `max` substitution they get on the
        // materialized path (AB#47592).
        assert_eq!(dae_streamed_declaration(SQL_LONGVARCHAR, 10), Ok(None));
    }

    /// A UTF-8 sequence split across two chunks is reassembled rather than
    /// decoding to replacement characters: chunk boundaries are chosen by the
    /// application and have nothing to do with character boundaries.
    #[test]
    fn transcode_carries_a_split_utf8_sequence_into_the_next_chunk() {
        let transcode = DaeTranscode::new(SQL_C_CHAR, SQL_WVARCHAR, SqlCollation::default());
        let mut carry = Vec::new();
        // "caf" + the first byte of U+00E9.
        let first = transcode.push(&mut carry, &[b'c', b'a', b'f', 0xC3]);
        assert_eq!(
            first,
            b"c\0a\0f\0".to_vec(),
            "the partial byte is held back"
        );
        let second = transcode.push(&mut carry, &[0xA9]);
        assert_eq!(second, vec![0xE9, 0x00], "the completed sequence follows");
        assert!(transcode.finish(&mut carry).is_empty());
    }

    /// Every UTF-8 sequence width can straddle a boundary, including a 4-byte
    /// one that becomes a surrogate pair.
    #[test]
    fn transcode_carries_every_utf8_sequence_width() {
        let transcode = DaeTranscode::new(SQL_C_CHAR, SQL_WVARCHAR, SqlCollation::default());
        for split in 1..4 {
            let mut carry = Vec::new();
            // U+1F600, four bytes, one surrogate pair.
            let full = "\u{1F600}".as_bytes();
            let mut out = transcode.push(&mut carry, &full[..split]);
            out.extend(transcode.push(&mut carry, &full[split..]));
            out.extend(transcode.finish(&mut carry));
            assert_eq!(
                out,
                vec![0x3D, 0xD8, 0x00, 0xDE],
                "split after {split} byte(s)"
            );
        }
    }

    /// A wide buffer feeding a narrow target has its own boundaries: half a
    /// code unit, and a surrogate pair split down the middle.
    ///
    /// Asserted as boundary-independence rather than against fixed bytes: what
    /// the collation makes of an unmappable character is the encoder's business
    /// (and matches the materialized path), but *where the chunks were split*
    /// must not change the answer.
    #[test]
    fn transcode_carries_a_split_utf16_unit() {
        let transcode = DaeTranscode::new(SQL_C_WCHAR, SQL_VARCHAR, SqlCollation::default());
        let mut carry = Vec::new();
        // U+00E9 as UTF-16LE, split between its two bytes.
        assert!(transcode.push(&mut carry, &[0xE9]).is_empty());
        assert_eq!(transcode.push(&mut carry, &[0x00]), vec![0xE9]);
        assert!(transcode.finish(&mut carry).is_empty());

        // A surrogate pair, whole and then split at every offset.
        let pair = [0x3D, 0xD8, 0x00, 0xDE];
        let mut whole_carry = Vec::new();
        let whole = transcode.push(&mut whole_carry, &pair);
        assert!(transcode.finish(&mut whole_carry).is_empty());
        for split in 1..pair.len() {
            let mut carry = Vec::new();
            let mut out = transcode.push(&mut carry, &pair[..split]);
            out.extend(transcode.push(&mut carry, &pair[split..]));
            out.extend(transcode.finish(&mut carry));
            assert_eq!(out, whole, "split after {split} byte(s) changed the value");
        }
    }

    /// A value that ends part-way through a character has no continuation
    /// coming, so the held bytes are flushed lossily rather than dropped.
    #[test]
    fn transcode_flushes_a_truncated_tail_as_a_replacement() {
        let transcode = DaeTranscode::new(SQL_C_CHAR, SQL_WVARCHAR, SqlCollation::default());
        let mut carry = Vec::new();
        assert!(transcode.push(&mut carry, &[0xC3]).is_empty());
        assert_eq!(transcode.finish(&mut carry), vec![0xFD, 0xFF], "U+FFFD");
    }

    /// A pairing whose buffer bytes are already the wire's bytes copies rather
    /// than decoding, so the large-value path keeps its cost.
    #[test]
    fn transcode_passes_matching_encodings_through() {
        let collation = SqlCollation::default();
        assert!(DaeTranscode::new(SQL_C_BINARY, SQL_VARBINARY, collation).is_passthrough());
        assert!(DaeTranscode::new(SQL_C_WCHAR, SQL_WVARCHAR, collation).is_passthrough());
        // A narrow buffer into a wide target always re-encodes.
        assert!(!DaeTranscode::new(SQL_C_CHAR, SQL_WVARCHAR, collation).is_passthrough());
    }

    /// `ColumnSize` bounds a streamed parameter as the chunks arrive, rather
    /// than being left to the declaration: the value body is PLP-framed and so
    /// carries no length of its own, and the bound has to be reported by the
    /// `SQLPutData` that breaches it (AB#47590).
    ///
    /// The unit is the declaration's, so the byte budget scales with the C
    /// buffer's element width - a `SQL_C_WCHAR` character is two bytes where a
    /// binary or narrow one is a single byte.
    #[test]
    fn dae_length_limit_scales_column_size_by_the_buffer_unit() {
        let limit = dae_length_limit(SQL_C_BINARY, SQL_VARBINARY, 2)
            .unwrap()
            .expect("a bounded varbinary is limited");
        assert_eq!(limit.bound, DaeBound::Bytes(2));
        assert_eq!(limit.pad_unit, [0u8]);

        // A narrow character buffer is measured in the UTF-16 units the
        // materialized path uses, not in its UTF-8 bytes.
        let limit = dae_length_limit(SQL_C_CHAR, SQL_VARCHAR, 4)
            .unwrap()
            .expect("a bounded varchar is limited");
        assert_eq!(limit.bound, DaeBound::Utf16Units(4));
        assert_eq!(limit.pad_unit, *b" ");

        // Four characters of nvarchar are eight bytes of SQL_C_WCHAR buffer.
        let limit = dae_length_limit(SQL_C_WCHAR, SQL_WVARCHAR, 4)
            .unwrap()
            .expect("a bounded nvarchar is limited");
        assert_eq!(limit.bound, DaeBound::Bytes(8));
        assert_eq!(limit.pad_unit, [b' ', 0]);
    }

    /// A `max` declaration has no length to enforce. `SQLDescribeParam` reports
    /// `ColumnSize` 0 for one, and a size past the non-`max` ceiling widens to
    /// `max` rather than erroring - the same reading [`variable_length`] gives
    /// the materialized path.
    #[test]
    fn dae_length_limit_is_absent_for_the_max_declarations() {
        for (c_type, sql_type, column_size) in [
            (SQL_C_BINARY, SQL_VARBINARY, 0),
            (SQL_C_CHAR, SQL_VARCHAR, 0),
            (SQL_C_WCHAR, SQL_WVARCHAR, 0),
            (SQL_C_BINARY, SQL_VARBINARY, SQL_PREC_BIGCHARBINARY + 1),
            (SQL_C_CHAR, SQL_VARCHAR, SQL_PREC_BIGCHARBINARY + 1),
            (SQL_C_WCHAR, SQL_WVARCHAR, SQL_PREC_NCHAR + 1),
        ] {
            assert_eq!(
                dae_length_limit(c_type, sql_type, column_size).unwrap(),
                None,
                "{c_type} -> {sql_type} at {column_size} should be unbounded"
            );
        }
    }

    /// `text`/`ntext`/`image` carry no declared length but are still bounded by
    /// `ColumnSize`, exactly as they are when the value is materialized.
    #[test]
    fn dae_length_limit_bounds_the_long_types_by_column_size() {
        let limit = dae_length_limit(SQL_C_BINARY, SQL_LONGVARBINARY, 3)
            .unwrap()
            .expect("image is bounded by ColumnSize");
        assert_eq!(limit.bound, DaeBound::Bytes(3));

        let limit = dae_length_limit(SQL_C_CHAR, SQL_LONGVARCHAR, 3)
            .unwrap()
            .expect("text is bounded by ColumnSize");
        assert_eq!(limit.bound, DaeBound::Utf16Units(3));
    }

    /// A zero `ColumnSize` is the `max` spelling for the variable-width types
    /// but invalid T-SQL for the fixed-width ones, which have no `max` form.
    /// The streamed path reads it the same way the materialized path does.
    #[test]
    fn dae_length_limit_rejects_a_zero_size_on_the_fixed_width_types() {
        for (c_type, sql_type) in [
            (SQL_C_BINARY, SQL_BINARY),
            (SQL_C_CHAR, SQL_CHAR),
            (SQL_C_WCHAR, SQL_WCHAR),
        ] {
            let err = dae_length_limit(c_type, sql_type, 0).unwrap_err();
            assert!(matches!(err, ParamBuildError::InvalidParameterSize(0)));
        }
    }

    /// The bound applies to the accumulated value, not to each chunk: an
    /// overflow can first appear on a chunk that is individually well inside
    /// the declaration. A per-chunk check would pass both of these.
    #[test]
    fn dae_limit_fit_measures_the_accumulated_total() {
        let limit = dae_length_limit(SQL_C_BINARY, SQL_VARBINARY, 4)
            .unwrap()
            .unwrap();

        // Each chunk is under the limit on its own; together they exceed it.
        assert_eq!(limit.fit(&[1, 2, 3], 0).unwrap(), (&[1, 2, 3][..], 3));
        assert_eq!(
            limit.fit(&[4, 5, 6], 3).unwrap_err(),
            ParamBuildError::StringTruncation
        );
    }

    /// An overflow made entirely of padding is dropped rather than reported -
    /// the streamed counterpart of [`trim_zero_overflow`] and
    /// [`trim_blank_overflow`]. Anything else in the overflow is `22001`,
    /// wherever in it the first non-pad byte sits.
    #[test]
    fn dae_limit_fit_trims_padding_and_reports_anything_else() {
        let binary = dae_length_limit(SQL_C_BINARY, SQL_VARBINARY, 2)
            .unwrap()
            .unwrap();
        assert_eq!(binary.fit(&[1, 2, 0, 0], 0).unwrap(), (&[1, 2][..], 2));
        assert_eq!(
            binary.fit(&[1, 2, 0, 3], 0).unwrap_err(),
            ParamBuildError::StringTruncation
        );

        let narrow = dae_length_limit(SQL_C_CHAR, SQL_VARCHAR, 2)
            .unwrap()
            .unwrap();
        assert_eq!(narrow.fit(b"ab  ", 0).unwrap(), (&b"ab"[..], 2));
        assert_eq!(
            narrow.fit(b"abcd", 0).unwrap_err(),
            ParamBuildError::StringTruncation
        );

        // The wide pad is a whole UTF-16 blank, so a lone 0x20 byte is not one.
        let wide = dae_length_limit(SQL_C_WCHAR, SQL_WVARCHAR, 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            wide.fit(&[b'a', 0, b' ', 0], 0).unwrap(),
            (&[b'a', 0][..], 2)
        );
        assert_eq!(
            wide.fit(&[b'a', 0, b'b', 0], 0).unwrap_err(),
            ParamBuildError::StringTruncation
        );
    }

    /// A `SQL_C_CHAR` buffer holds UTF-8, so its bytes are not its units. The
    /// materialized path measures every character input in UTF-16 units
    /// (`trim_blank_overflow`), and counting bytes here instead would reject a
    /// value the same binding materialized accepts -- one non-ASCII character
    /// against `varchar(1)` is two bytes but one unit.
    #[test]
    fn dae_limit_fit_measures_a_narrow_buffer_in_utf16_units() {
        let limit = dae_length_limit(SQL_C_CHAR, SQL_VARCHAR, 1)
            .unwrap()
            .unwrap();
        // U+00E9, two UTF-8 bytes, one UTF-16 unit: it fits `varchar(1)`.
        assert_eq!(limit.fit("é".as_bytes(), 0).unwrap(), ("é".as_bytes(), 1));

        let limit = dae_length_limit(SQL_C_CHAR, SQL_VARCHAR, 4)
            .unwrap()
            .unwrap();
        // Four characters, six bytes: a byte count would have rejected these.
        assert_eq!(limit.fit("café".as_bytes(), 0).unwrap().1, 4);
        // U+1D11E is four UTF-8 bytes and a surrogate pair, so it costs two of
        // the four units, exactly as the materialized path counts it.
        assert_eq!(limit.fit("𝄞ab".as_bytes(), 0).unwrap().1, 4);
    }

    /// The unit count is taken from each byte on its own, so a character split
    /// across two `SQLPutData` calls is counted once, at its lead, without the
    /// limit carrying any decode state between chunks.
    #[test]
    fn dae_limit_fit_counts_a_split_character_once() {
        let limit = dae_length_limit(SQL_C_CHAR, SQL_VARCHAR, 2)
            .unwrap()
            .unwrap();
        // "é" arrives as its lead byte and then its continuation byte.
        let (_, first) = limit.fit(&[0xC3], 0).unwrap();
        assert_eq!(first, 1, "the lead pays for the character");
        let (_, second) = limit.fit(&[0xA9], first).unwrap();
        assert_eq!(second, 0, "the continuation is already paid for");
        // One unit consumed in total, so a second character still fits.
        assert_eq!(limit.fit(b"z", first + second).unwrap().1, 1);
    }

    /// The pairings `park_dae_client` leaves without a transcode, so
    /// `SQLPutData` forwards their chunks borrowed instead of copying each one
    /// through a conversion that returns them unchanged. Binary is the case
    /// that matters most: streaming a large `varbinary(max)` is the canonical
    /// reason to use data-at-execution at all, and it never needs conversion.
    #[test]
    fn a_passthrough_pairing_needs_no_transcode() {
        let collation = SqlCollation::default();
        for (c_type, sql_type) in [
            (SQL_C_BINARY, SQL_VARBINARY),
            (SQL_C_BINARY, SQL_LONGVARBINARY),
            (SQL_C_WCHAR, SQL_WVARCHAR),
        ] {
            assert!(
                DaeTranscode::new(c_type, sql_type, collation).is_passthrough(),
                "{c_type} -> {sql_type} is already the wire's bytes"
            );
        }

        // A wideness mismatch does convert, so it keeps its transcode.
        assert!(
            !DaeTranscode::new(SQL_C_WCHAR, SQL_VARCHAR, collation).is_passthrough(),
            "a wide buffer against a narrow target has to be re-encoded"
        );
    }

    /// A wideness mismatch inside the character family *is* measurable, because
    /// the unit is the declaration's and the count is of the source: both
    /// `varchar(n)` and `nvarchar(n)` bound n UTF-16 units, exactly as
    /// `convert_character_sql` bounds them on the materialized path. Only the
    /// buffer's own width changes how those units are counted.
    #[test]
    fn dae_length_limit_measures_a_wideness_mismatch() {
        // UTF-8 source against a wide declaration: counted in UTF-16 units.
        for sql_type in [SQL_WVARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR] {
            assert_eq!(
                dae_length_limit(SQL_C_CHAR, sql_type, 10).unwrap(),
                Some(DaeLengthLimit {
                    bound: DaeBound::Utf16Units(10),
                    pad_unit: b" ",
                }),
                "SQL_C_CHAR -> {sql_type} bounds 10 UTF-16 units of UTF-8"
            );
        }

        // UTF-16 source against a narrow declaration: a unit is a fixed two
        // bytes, so the byte bound is exact.
        for sql_type in [SQL_VARCHAR, SQL_CHAR, SQL_LONGVARCHAR] {
            assert_eq!(
                dae_length_limit(SQL_C_WCHAR, sql_type, 10).unwrap(),
                Some(DaeLengthLimit {
                    bound: DaeBound::Bytes(20),
                    pad_unit: &[b' ', 0],
                }),
                "SQL_C_WCHAR -> {sql_type} bounds 10 units as 20 buffer bytes"
            );
        }
    }

    /// Cross-*family* is the pairing that genuinely cannot be measured here: a
    /// binary byte is not a character, so the two sides count different things
    /// and the bound is left to the close-time conversion.
    #[test]
    fn dae_length_limit_is_absent_across_families() {
        for (c_type, sql_type) in [
            (SQL_C_BINARY, SQL_VARCHAR),
            (SQL_C_BINARY, SQL_WVARCHAR),
            (SQL_C_CHAR, SQL_VARBINARY),
            (SQL_C_WCHAR, SQL_VARBINARY),
        ] {
            assert_eq!(
                dae_length_limit(c_type, sql_type, 10).unwrap(),
                None,
                "{c_type} -> {sql_type} counts different things on each side"
            );
        }
    }

    /// Once the budget is spent every later chunk must be padding, and the
    /// trimmed length is what the total advances by - so padding dropped at the
    /// boundary does not consume the declaration's remaining room.
    #[test]
    fn dae_limit_fit_admits_only_padding_past_the_budget() {
        let limit = dae_length_limit(SQL_C_BINARY, SQL_VARBINARY, 2)
            .unwrap()
            .unwrap();
        assert!(limit.fit(&[0, 0], 2).unwrap().0.is_empty());
        assert_eq!(
            limit.fit(&[0, 1], 2).unwrap_err(),
            ParamBuildError::StringTruncation
        );
    }

    /// A pad unit split across two `SQLPutData` calls is still padding. The
    /// first chunk can end on the `0x20` of a UTF-16 blank whose `0x00` arrives
    /// next, so a trailing partial unit that is a prefix of the pad is trimmed
    /// with the rest of it rather than reported - rejecting it would fail a
    /// value whose overflow is entirely blanks purely because of where the chunk
    /// boundary fell. A partial unit that is *not* a pad prefix is still
    /// `22001`.
    #[test]
    fn dae_limit_fit_accepts_a_pad_unit_split_across_chunks() {
        let wide = dae_length_limit(SQL_C_WCHAR, SQL_WVARCHAR, 1)
            .unwrap()
            .unwrap();
        // "a" fills the bound; the trailing 0x20 begins a blank.
        assert_eq!(
            wide.fit(&[b'a', 0, b' '], 0).unwrap(),
            (&[b'a', 0][..], 2),
            "a split blank is padding, not truncation"
        );
        // A partial unit that cannot become a blank is still reported.
        assert_eq!(
            wide.fit(&[b'a', 0, b'z'], 0).unwrap_err(),
            ParamBuildError::StringTruncation
        );
    }

    /// Every pairing the materialized path can convert is now supplyable at
    /// execution: the ones whose encoding cannot be settled chunk-wise buffer
    /// and take that path rather than being refused (AB#47590).
    #[test]
    fn cross_family_dae_is_accepted() {
        // A cross-family pairing whose target is PLP-framable streams, with the
        // encoding difference handled per chunk by `DaeTranscode`.
        for (c_type, sql_type) in [
            (SQL_C_CHAR, SQL_WVARCHAR),
            (SQL_C_CHAR, SQL_WLONGVARCHAR),
            (SQL_C_WCHAR, SQL_VARCHAR),
            (SQL_C_WCHAR, SQL_LONGVARCHAR),
            (SQL_C_BINARY, SQL_VARCHAR),
            (SQL_C_CHAR, SQL_VARBINARY),
        ] {
            assert!(
                matches!(dae_plan(c_type, sql_type), Ok(DaePlan::Stream(_))),
                "{c_type} -> {sql_type} should stream"
            );
        }

        // The rest are collected and converted whole, rather than refused as
        // they were before.
        for (c_type, sql_type) in [
            (SQL_C_CHAR, SQL_WCHAR),
            (SQL_C_WCHAR, SQL_CHAR),
            (SQL_C_CHAR, SQL_INTEGER),
            (SQL_C_WCHAR, SQL_BIGINT),
        ] {
            assert_eq!(
                dae_plan(c_type, sql_type),
                Ok(DaePlan::Buffer),
                "{c_type} -> {sql_type} should buffer"
            );
        }

        // An unsupported wire type is still refused, since no conversion exists
        // for it on either path.
        let err = dae_plan(SQL_C_CHAR, SQL_SS_VECTOR).unwrap_err();
        assert_eq!(err.diag().state, ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED.state);
    }

    /// The pairing this driver used to reject with `HYC00`: a wide C type
    /// streamed against a narrow SQL type, or the reverse. mssql-python binds
    /// every character parameter's data-at-execution path as `SQL_C_WCHAR`,
    /// including narrow (ASCII) values, so this is the ordinary shape for any
    /// bound string over ~4000 characters (AB#47709's follow-up).
    ///
    /// It streams like any other PLP-framable pairing; the encoding difference
    /// is handled per chunk by [`DaeTranscode`] rather than by refusing it.
    #[test]
    fn wideness_mismatched_dae_streams_and_transcodes() {
        for (c_type, sql_type, streamed) in [
            (SQL_C_WCHAR, SQL_VARCHAR, StreamedSqlType::VarcharMax),
            (SQL_C_WCHAR, SQL_LONGVARCHAR, StreamedSqlType::VarcharMax),
            (SQL_C_CHAR, SQL_WVARCHAR, StreamedSqlType::NVarcharMax),
            (SQL_C_CHAR, SQL_WLONGVARCHAR, StreamedSqlType::NVarcharMax),
        ] {
            assert_eq!(
                dae_plan(c_type, sql_type),
                Ok(DaePlan::Stream(streamed)),
                "{c_type} -> {sql_type}"
            );
            assert!(
                !DaeTranscode::new(c_type, sql_type, utf8_collation()).is_passthrough(),
                "{c_type} -> {sql_type} must re-encode rather than pass through"
            );
        }
    }

    /// The wire type follows the *declared* `ParameterType`, not the C type --
    /// otherwise a narrow column would receive UTF-16LE bytes under a wide
    /// declaration, or vice versa, regardless of transcoding.
    #[test]
    fn transcode_wide_round_trips() {
        let wide_bytes: Vec<u8> = "caf\u{e9}"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let transcode = DaeTranscode::new(SQL_C_WCHAR, SQL_VARCHAR, utf8_collation());
        let mut carry = Vec::new();
        let mut narrow = transcode.push(&mut carry, &wide_bytes);
        narrow.extend(transcode.finish(&mut carry));
        assert_eq!(String::from_utf8(narrow).unwrap(), "caf\u{e9}");

        let transcode = DaeTranscode::new(SQL_C_CHAR, SQL_WVARCHAR, utf8_collation());
        let mut carry = Vec::new();
        let mut wide = transcode.push(&mut carry, "caf\u{e9}".as_bytes());
        wide.extend(transcode.finish(&mut carry));
        let wide_units: Vec<u16> = wide
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        assert_eq!(String::from_utf16(&wide_units).unwrap(), "caf\u{e9}");
    }

    /// The narrow direction has to land on the *connection's* collation, not
    /// this driver's own UTF-8 C-side convention -- otherwise a non-ASCII
    /// value silently corrupts under any collation that isn't itself UTF-8.
    /// Windows-1252 encodes 'é' as the single byte 0xE9; naive UTF-8
    /// passthrough would instead send 0xC3 0xA9, mojibake to a server
    /// expecting Windows-1252.
    #[test]
    fn transcode_narrow_uses_the_connection_collation() {
        let wide_bytes: Vec<u8> = "caf\u{e9}"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let transcode = DaeTranscode::new(SQL_C_WCHAR, SQL_VARCHAR, windows_1252_collation());
        let mut carry = Vec::new();
        let mut narrow = transcode.push(&mut carry, &wide_bytes);
        narrow.extend(transcode.finish(&mut carry));
        assert_eq!(narrow, b"caf\xe9");

        // Same for a narrow C buffer: `SQL_C_CHAR` is UTF-8 by this driver's
        // convention, which is not a wire encoding.
        let transcode = DaeTranscode::new(SQL_C_CHAR, SQL_VARCHAR, windows_1252_collation());
        let mut carry = Vec::new();
        let mut narrow = transcode.push(&mut carry, "caf\u{e9}".as_bytes());
        narrow.extend(transcode.finish(&mut carry));
        assert_eq!(narrow, b"caf\xe9");
    }

    fn utf8_collation() -> SqlCollation {
        SqlCollation {
            info: 0,
            lcid_language_id: 0,
            col_flags: 0x40, // fUTF8
            sort_id: 0,
        }
    }

    fn windows_1252_collation() -> SqlCollation {
        SqlCollation {
            info: 0x0409, // US English LCID -> Windows-1252
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        }
    }

    /// A parameter that reaches value conversion still carrying a
    /// data-at-execution indicator was never staged for streaming. That is a
    /// driver bug, so it maps to a driver error rather than "not implemented".
    #[test]
    fn data_at_exec_not_staged_maps_to_a_driver_error() {
        assert_eq!(
            ParamBuildError::DataAtExecNotStaged.diag().state,
            ERR_DATA_AT_EXEC_NOT_STAGED.state
        );
    }

    #[test]
    fn wchar_explicit_length_becomes_nvarchar() {
        let mut buf: Vec<u16> = "hi".encode_utf16().collect();
        let mut ind: SqlLen = (buf.len() * 2) as SqlLen;
        let mut p = param(SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_WVARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::NVarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hi"),
            other => panic!("expected NVarcharMax(Some), got {other:?}"),
        }
    }

    /// Binds `bytes` under `sql_type`/`column_size` and returns the wire value.
    fn convert_binary(
        sql_type: SqlSmallInt,
        column_size: usize,
        bytes: &[u8],
    ) -> Result<SqlType, ParamBuildError> {
        let mut buf = bytes.to_vec();
        let mut ind: SqlLen = buf.len() as SqlLen;
        let mut p = param(SQL_C_BINARY, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = sql_type;
        p.column_size = column_size;
        unsafe { bound_param_to_value(&p) }.map(|(value, _)| value)
    }

    /// The wire type follows `ParameterType` and `ColumnSize`, as it does for
    /// the character families - previously every binary value went out as
    /// `varbinary(max)` whatever the application declared.
    #[test]
    fn parameter_type_names_the_binary_wire_type() {
        let value = convert_binary(SQL_BINARY, 4, &[1, 2, 3, 4]).unwrap();
        assert_eq!(value, SqlType::Binary(Some(vec![1, 2, 3, 4]), 4));

        let value = convert_binary(SQL_VARBINARY, 4, &[1, 2]).unwrap();
        assert_eq!(value, SqlType::VarBinary(Some(vec![1, 2]), 4));

        // 0 is the `max` spelling for the variable-length type, as it is for
        // `varchar`/`nvarchar`.
        let value = convert_binary(SQL_VARBINARY, 0, &[1, 2]).unwrap();
        assert_eq!(value, SqlType::VarBinaryMax(Some(vec![1, 2])));

        // `image` has no declared length; AB#47592 sends it as `max`.
        let value = convert_binary(SQL_LONGVARBINARY, 16, &[1, 2]).unwrap();
        assert_eq!(value, SqlType::VarBinaryMax(Some(vec![1, 2])));
    }

    /// Overflow that is entirely `0x00` is padding and is dropped silently -
    /// `CheckTrailingZeros` (`sqlccnvt.cpp:8690`) returning FALSE at
    /// `sqlcfunc.cpp:2611`. The blank-padding character rule, one byte value over.
    #[test]
    fn an_all_zero_binary_overflow_is_trimmed_silently() {
        let value = convert_binary(SQL_VARBINARY, 4, &[1, 2, 3, 4, 0, 0, 0]).unwrap();
        assert_eq!(value, SqlType::VarBinary(Some(vec![1, 2, 3, 4]), 4));

        let value = convert_binary(SQL_BINARY, 2, &[0xAB, 0xCD, 0, 0]).unwrap();
        assert_eq!(value, SqlType::Binary(Some(vec![0xAB, 0xCD]), 2));

        // `image` is the only arm where the declared type is unbounded and the
        // enforced bound is not, so the trim has to be asserted separately from
        // the declaration.
        let value = convert_binary(SQL_LONGVARBINARY, 2, &[1, 2, 0, 0]).unwrap();
        assert_eq!(value, SqlType::VarBinaryMax(Some(vec![1, 2])));
    }

    /// Any non-zero byte past the declared length is data, so it is `22001`
    /// rather than a silent loss.
    #[test]
    fn a_binary_overflow_carrying_data_is_22001() {
        assert_eq!(
            convert_binary(SQL_VARBINARY, 4, &[1, 2, 3, 4, 5]),
            Err(ParamBuildError::StringTruncation)
        );
        assert_eq!(
            convert_binary(SQL_BINARY, 2, &[1, 2, 3]),
            Err(ParamBuildError::StringTruncation)
        );
        assert_eq!(
            convert_binary(SQL_LONGVARBINARY, 2, &[1, 2, 3]),
            Err(ParamBuildError::StringTruncation)
        );
    }

    /// The scan stops at the first non-pad byte wherever it sits, so an overflow
    /// that is *mostly* zero still errors. The character equivalent of this case
    /// was missed once and only caught by mutation testing.
    #[test]
    fn a_partially_zero_binary_overflow_is_22001() {
        // Non-zero byte first in the overflow, then padding.
        assert_eq!(
            convert_binary(SQL_VARBINARY, 3, &[1, 2, 3, 9, 0, 0]),
            Err(ParamBuildError::StringTruncation)
        );
        // Padding first, then a non-zero byte at the very end.
        assert_eq!(
            convert_binary(SQL_VARBINARY, 3, &[1, 2, 3, 0, 0, 9]),
            Err(ParamBuildError::StringTruncation)
        );
        // A non-zero byte in the middle of the overflow.
        assert_eq!(
            convert_binary(SQL_VARBINARY, 3, &[1, 2, 3, 0, 9, 0]),
            Err(ParamBuildError::StringTruncation)
        );
    }

    /// A value at exactly the declared length is untouched, and a zero *inside*
    /// it is data rather than padding.
    #[test]
    fn a_binary_value_that_fits_keeps_its_interior_zeros() {
        let value = convert_binary(SQL_VARBINARY, 4, &[1, 0, 0, 2]).unwrap();
        assert_eq!(value, SqlType::VarBinary(Some(vec![1, 0, 0, 2]), 4));

        // Shorter than the declaration is not padded anywhere in this driver:
        // `is_fixed_length` is false for every RPC type, so `serialize_bytes`
        // sends the value's own length and the server pads to `binary(n)`.
        let value = convert_binary(SQL_BINARY, 4, &[1, 2]).unwrap();
        assert_eq!(value, SqlType::Binary(Some(vec![1, 2]), 4));
    }

    /// An empty buffer never overflows. msodbcsql guards the whole check with
    /// `cbData > 0` (`sqlcfunc.cpp:2603`), which only bites when the bound is
    /// itself 0; no binding reaches this function that way, since `SQL_VARBINARY`
    /// 0 means `max` and the other two reject 0 at bind. The second case below
    /// constructs that unreachable pairing directly, so the zero-bound arm is
    /// covered even though the API cannot produce it.
    #[test]
    fn an_empty_binary_value_is_never_truncated() {
        let value = convert_binary(SQL_VARBINARY, 4, &[]).unwrap();
        assert_eq!(value, SqlType::VarBinary(Some(Vec::new()), 4));

        let value = convert_binary(SQL_LONGVARBINARY, 0, &[]).unwrap();
        assert_eq!(value, SqlType::VarBinaryMax(Some(Vec::new())));
    }

    /// `ColumnSize` past the non-`max` bound widens to `max` here, matching
    /// `variable_length` and `RpcParameter::get_sql_name`.
    ///
    /// **Not reachable through the API**: `parameter_column_size_is_valid` caps
    /// `SQL_VARBINARY` at 8000, so `SQLBindParameter` answers `HY104` first - as
    /// `ColumnSizeAtTheNonMaxBoundary` asserts end to end. This pins the
    /// converter's own contract, nothing more.
    #[test]
    fn an_oversized_varbinary_column_size_widens_to_max() {
        let value = convert_binary(SQL_VARBINARY, SQL_PREC_BIGCHARBINARY, &[1, 2]).unwrap();
        assert_eq!(value, SqlType::VarBinary(Some(vec![1, 2]), 8000));

        let value = convert_binary(SQL_VARBINARY, SQL_PREC_BIGCHARBINARY + 1, &[1, 2]).unwrap();
        assert_eq!(value, SqlType::VarBinaryMax(Some(vec![1, 2])));
    }

    /// `binary(0)` is not legal T-SQL and has no `max` spelling, so it is
    /// `HY104` - the same line `char`/`nchar` draw, and the same one msodbcsql
    /// draws at bind time (`sqlcdesc.cpp:11783` groups `SQL_BINARY` with
    /// `SQL_CHAR` through `CheckSqlPrec`).
    #[test]
    fn a_zero_column_size_on_fixed_binary_is_rejected() {
        assert_eq!(
            convert_binary(SQL_BINARY, 0, &[1, 2]),
            Err(ParamBuildError::InvalidParameterSize(0))
        );
        assert_eq!(
            convert_binary(SQL_BINARY, SQL_PREC_BIGCHARBINARY + 1, &[1, 2]),
            Err(ParamBuildError::InvalidParameterSize(
                SQL_PREC_BIGCHARBINARY + 1
            ))
        );
    }

    /// Binds `text` under `sql_type`/`column_size` and returns the wire value.
    fn convert_char(
        c_type: SqlSmallInt,
        sql_type: SqlSmallInt,
        column_size: usize,
        text: &str,
    ) -> Result<SqlType, ParamBuildError> {
        let mut narrow: Vec<u8>;
        let mut wide: Vec<u16>;
        let (ptr, byte_len) = if c_type == SQL_C_WCHAR {
            wide = text.encode_utf16().collect();
            let len = wide.len() * size_of::<u16>();
            (wide.as_mut_ptr() as *mut c_void, len)
        } else {
            narrow = text.as_bytes().to_vec();
            let len = narrow.len();
            (narrow.as_mut_ptr() as *mut c_void, len)
        };
        let mut ind: SqlLen = byte_len as SqlLen;
        let mut p = param(c_type, ptr, &mut ind);
        p.sql_type = sql_type;
        p.column_size = column_size;
        unsafe { bound_param_to_value(&p) }.map(|(value, _)| value)
    }

    /// One buffer, six declarations: as with the integer quadrant, the wire type
    /// follows `ParameterType`, not the C type that happened to be bound.
    #[test]
    fn parameter_type_names_the_character_wire_type() {
        let cases: &[(SqlSmallInt, SqlType)] = &[
            (SQL_CHAR, SqlType::Char(None, 8)),
            (SQL_VARCHAR, SqlType::Varchar(None, 8)),
            (SQL_LONGVARCHAR, SqlType::VarcharMax(None)),
            (SQL_WCHAR, SqlType::NChar(None, 8)),
            (SQL_WVARCHAR, SqlType::NVarchar(None, 8)),
            (SQL_WLONGVARCHAR, SqlType::NVarcharMax(None)),
        ];
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
            for (sql_type, expected) in cases {
                let value = convert_char(c_type, *sql_type, 8, "abc").unwrap();
                assert_eq!(
                    std::mem::discriminant(&value),
                    std::mem::discriminant(expected),
                    "{c_type} -> {sql_type} named the wrong wire type: {value:?}"
                );
            }
        }
    }

    /// A cross-family pairing transcodes instead of being rejected, and the
    /// payload survives the round trip in both directions.
    #[test]
    fn cross_family_pairings_transcode() {
        let text = "caf\u{e9} \u{2615}";
        match convert_char(SQL_C_CHAR, SQL_WVARCHAR, 0, text).unwrap() {
            SqlType::NVarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), text),
            other => panic!("expected NVarcharMax(Some), got {other:?}"),
        }
        match convert_char(SQL_C_WCHAR, SQL_VARCHAR, 0, text).unwrap() {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), text),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    /// `ColumnSize == 0` is the unbounded sentinel, so a `varchar` parameter
    /// widens to `varchar(max)` rather than declaring a zero length.
    #[test]
    fn unbounded_column_size_selects_the_max_types() {
        assert!(matches!(
            convert_char(SQL_C_CHAR, SQL_VARCHAR, 0, "abc").unwrap(),
            SqlType::VarcharMax(Some(_))
        ));
        assert!(matches!(
            convert_char(SQL_C_WCHAR, SQL_WVARCHAR, 0, "abc").unwrap(),
            SqlType::NVarcharMax(Some(_))
        ));
    }

    /// `ColumnSize` past the non-`max` ceiling also means `max`, and the ceiling
    /// itself still declares a bounded length.
    ///
    /// The `+ 1` cases pin the internal contract only: `SQLBindParameter`
    /// rejects them with `HY104` first (`parameter_column_size_is_valid`), so
    /// they are unreachable through the API. `variable_length` is deliberately
    /// laxer than the gate - see its doc comment.
    #[test]
    fn column_size_at_and_past_the_ceiling() {
        assert!(matches!(
            convert_char(SQL_C_CHAR, SQL_VARCHAR, SQL_PREC_BIGCHARBINARY, "abc").unwrap(),
            SqlType::Varchar(_, 8000)
        ));
        assert!(matches!(
            convert_char(SQL_C_CHAR, SQL_VARCHAR, SQL_PREC_BIGCHARBINARY + 1, "abc").unwrap(),
            SqlType::VarcharMax(_)
        ));
        assert!(matches!(
            convert_char(SQL_C_WCHAR, SQL_WVARCHAR, SQL_PREC_NCHAR, "abc").unwrap(),
            SqlType::NVarchar(_, 4000)
        ));
        assert!(matches!(
            convert_char(SQL_C_WCHAR, SQL_WVARCHAR, SQL_PREC_NCHAR + 1, "abc").unwrap(),
            SqlType::NVarcharMax(_)
        ));
    }

    /// Overflow that is not all blanks is an error, not a silent trim.
    #[test]
    fn overlong_value_is_22001() {
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
            for sql_type in [SQL_CHAR, SQL_VARCHAR, SQL_WCHAR, SQL_WVARCHAR] {
                let err = convert_char(c_type, sql_type, 3, "abcd").unwrap_err();
                assert!(
                    matches!(err, ParamBuildError::StringTruncation),
                    "{c_type} -> {sql_type} gave {err:?}"
                );
                assert_eq!(err.diag().state, *b"22001");
            }
        }
    }

    /// The cross-family boundary, which `overlong_value_is_22001` only pins from
    /// the failing side: exactly at the limit converts, one unit past is `22001`.
    #[test]
    fn cross_family_boundary_is_exact() {
        for (c_type, sql_type) in [
            (SQL_C_CHAR, SQL_WVARCHAR),
            (SQL_C_WCHAR, SQL_VARCHAR),
            (SQL_C_CHAR, SQL_WCHAR),
            (SQL_C_WCHAR, SQL_CHAR),
        ] {
            assert!(
                convert_char(c_type, sql_type, 3, "abc").is_ok(),
                "{c_type} -> {sql_type} rejected a value that exactly fits"
            );
            assert_eq!(
                convert_char(c_type, sql_type, 3, "abcd").unwrap_err(),
                ParamBuildError::StringTruncation,
                "{c_type} -> {sql_type} accepted one unit past the limit"
            );
        }
    }

    /// An astral character is two UTF-16 units and four UTF-8 bytes, so a wide
    /// source measured against a narrow target passes at two units and ships
    /// four bytes. Harmless under a single-byte collation, which cannot encode
    /// it anyway, but on a `_UTF8` database the value the server sizes is twice
    /// what was validated (AB#47584).
    #[test]
    fn an_astral_char_costs_two_units_but_four_bytes() {
        let emoji = "\u{1F600}";
        match convert_char(SQL_C_WCHAR, SQL_CHAR, 2, emoji).unwrap() {
            SqlType::Char(Some(s), 2) => {
                assert_eq!(s.to_utf8_string(), emoji);
                assert_eq!(s.to_utf8_string().len(), 4);
            }
            other => panic!("expected Char(Some, 2), got {other:?}"),
        }
        assert_eq!(
            convert_char(SQL_C_WCHAR, SQL_CHAR, 1, emoji).unwrap_err(),
            ParamBuildError::StringTruncation
        );
    }

    /// Malformed narrow input is repaired before it is measured, so the count
    /// covers the U+FFFD it will become - but a value that fits in units can
    /// still exceed the declared length in collation bytes, since each U+FFFD
    /// occupies three. The narrow mirror of
    /// `a_lone_surrogate_is_measured_before_repair` (AB#47584).
    #[test]
    fn invalid_utf8_at_the_limit_grows_past_it() {
        // Three bytes into a limit of three never reaches the measurement: the
        // byte-count short-circuit settles it, because no UTF-8 buffer yields
        // more UTF-16 units than bytes. The growth still happens on the wire.
        let mut buf: Vec<u8> = vec![b'a', b'b', 0xFF];
        let mut ind: SqlLen = buf.len() as SqlLen;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_VARCHAR;
        p.column_size = 3;

        match unsafe { bound_param_to_value(&p) }.unwrap().0 {
            SqlType::Varchar(Some(s), 3) => {
                assert_eq!(s.to_utf8_string(), "ab\u{FFFD}");
                assert_eq!(s.to_utf8_string().len(), 5);
            }
            other => panic!("expected Varchar(Some, 3), got {other:?}"),
        }

        // One byte longer does reach the measurement, and it counts the repaired
        // form: four bytes become four UTF-16 units, one past the limit, and the
        // overflowing byte is not a blank.
        let mut buf: Vec<u8> = vec![b'a', b'b', b'c', 0xFF];
        let mut ind: SqlLen = buf.len() as SqlLen;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_VARCHAR;
        p.column_size = 3;
        assert_eq!(
            unsafe { bound_param_to_value(&p) },
            Err(ParamBuildError::StringTruncation)
        );
    }

    /// Overflowing *blanks* are dropped without a diagnostic - msodbcsql checks
    /// the overflow with `CheckTrailingChars` before raising `22001`.
    #[test]
    fn overflowing_blanks_are_trimmed_silently() {
        match convert_char(SQL_C_CHAR, SQL_VARCHAR, 3, "abc   ").unwrap() {
            SqlType::Varchar(Some(s), 3) => assert_eq!(s.to_utf8_string(), "abc"),
            other => panic!("expected Varchar(Some, 3), got {other:?}"),
        }
        match convert_char(SQL_C_WCHAR, SQL_WVARCHAR, 3, "abc ").unwrap() {
            SqlType::NVarchar(Some(s), 3) => assert_eq!(s.to_utf8_string(), "abc"),
            other => panic!("expected NVarchar(Some, 3), got {other:?}"),
        }
    }

    /// The blank check covers the *whole* overflow, not just its first unit. An
    /// overflow that merely starts with a blank is still truncation; dropping it
    /// would lose the non-blank tail silently. Without this the every-test-blank
    /// or every-test-non-blank split leaves the `any` untested as a whole-region
    /// check - weakening it to inspect one unit passes the rest of the suite.
    #[test]
    fn a_partially_blank_overflow_is_truncation() {
        for (c_type, sql_type) in [
            (SQL_C_CHAR, SQL_VARCHAR),
            (SQL_C_CHAR, SQL_WVARCHAR),
            (SQL_C_WCHAR, SQL_WVARCHAR),
            (SQL_C_WCHAR, SQL_VARCHAR),
        ] {
            assert_eq!(
                convert_char(c_type, sql_type, 2, "ab c").unwrap_err(),
                ParamBuildError::StringTruncation,
                "c_type {c_type} -> sql_type {sql_type}"
            );
        }
    }

    /// Trimming a narrow source mixes units - `overflow` counts UTF-16 units,
    /// `keep` is a byte offset - which holds only because a blank is one byte
    /// and never a UTF-8 continuation byte. Multibyte content is what would
    /// expose it, against either target family.
    #[test]
    fn a_multibyte_narrow_source_trims_blanks_on_a_char_boundary() {
        match convert_char(SQL_C_CHAR, SQL_WVARCHAR, 1, "\u{e9}   ").unwrap() {
            SqlType::NVarchar(Some(s), 1) => assert_eq!(s.to_utf8_string(), "\u{e9}"),
            other => panic!("expected NVarchar(Some, 1), got {other:?}"),
        }
        match convert_char(SQL_C_CHAR, SQL_VARCHAR, 1, "\u{e9}   ").unwrap() {
            SqlType::Varchar(Some(s), 1) => assert_eq!(s.to_utf8_string(), "\u{e9}"),
            other => panic!("expected Varchar(Some, 1), got {other:?}"),
        }
    }

    /// A wide source is measured in its own UTF-16 units for *both* families -
    /// msodbcsql assumes one byte per `WCHAR` for a narrow target rather than
    /// encoding to find out. Counting the transcoded UTF-8 instead would falsely
    /// reject values that fit: `varchar(n)` bounds collation bytes, and under a
    /// single-byte collation "caf\u{e9}" is four of them, not five. The narrow
    /// source is now held to the same unit for the same reason.
    #[test]
    fn a_wide_source_is_measured_in_utf16_units_for_both_families() {
        let text = "caf\u{e9}";
        assert!(convert_char(SQL_C_WCHAR, SQL_VARCHAR, 4, text).is_ok());
        assert!(convert_char(SQL_C_WCHAR, SQL_WVARCHAR, 4, text).is_ok());
        assert_eq!(
            convert_char(SQL_C_WCHAR, SQL_VARCHAR, 3, text).unwrap_err(),
            ParamBuildError::StringTruncation
        );
    }

    /// Both character C types measure the same value alike, so a binding one
    /// accepts the other cannot reject.
    ///
    /// Counting a narrow source's UTF-8 bytes put them in disagreement:
    /// "caf\u{e9}" fitted a `varchar(4)` as `SQL_C_WCHAR` and was `22001` as
    /// `SQL_C_CHAR`, on data the server accepts - bound one character longer it
    /// reaches the server and `LEN` returns 4.
    #[test]
    fn both_character_c_types_measure_a_value_alike() {
        // The astral case is why the unit is UTF-16 rather than `char`: one
        // character but two units, so counting characters would have left the
        // two C types disagreeing here instead.
        for text in ["caf\u{e9}", "\u{2615}\u{2615}\u{2615}", "\u{1F600}"] {
            let units = text.encode_utf16().count();
            for sql_type in [SQL_VARCHAR, SQL_WVARCHAR] {
                for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
                    assert!(
                        convert_char(c_type, sql_type, units, text).is_ok(),
                        "{c_type} -> {sql_type} rejected {text:?} at its own unit count"
                    );
                    assert_eq!(
                        convert_char(c_type, sql_type, units - 1, text).unwrap_err(),
                        ParamBuildError::StringTruncation,
                        "{c_type} -> {sql_type} accepted {text:?} one unit past the limit"
                    );
                }
            }
        }
    }

    /// Malformed narrow input is repaired lossily rather than reported. Pinned
    /// so a later switch to `22018` registers as a behaviour change (AB#47565).
    #[test]
    fn malformed_utf8_is_repaired_lossily() {
        let mut buf: Vec<u8> = vec![b'a', 0xFF, b'b'];
        let mut ind: SqlLen = buf.len() as SqlLen;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_VARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "a\u{FFFD}b"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    /// A surrogate pair costs two units, which is what `nvarchar(n)` counts.
    #[test]
    fn a_surrogate_pair_costs_two_units() {
        let emoji = "\u{1F600}";
        assert_eq!(
            convert_char(SQL_C_WCHAR, SQL_WVARCHAR, 1, emoji).unwrap_err(),
            ParamBuildError::StringTruncation
        );
        match convert_char(SQL_C_WCHAR, SQL_WVARCHAR, 2, emoji).unwrap() {
            SqlType::NVarchar(Some(s), 2) => assert_eq!(s.to_utf8_string(), emoji),
            other => panic!("expected NVarchar(Some, 2), got {other:?}"),
        }
    }

    /// A lone surrogate is counted before it is repaired, so the unit we measure
    /// is not always the unit that ships: a wide target passes it through, while
    /// a narrow one replaces it with U+FFFD - one UTF-16 unit in, three UTF-8
    /// bytes out. The wide-source mirror of `malformed_utf8_is_repaired_lossily`,
    /// and reachable from any application that binds one.
    ///
    /// The narrow half is also the collation gap in miniature: three UTF-8 bytes
    /// in a `varchar(1)`, which only fits once `serialize_string` folds them to a
    /// single-byte code page.
    #[test]
    fn a_lone_surrogate_is_measured_before_repair() {
        let mut units: Vec<u16> = vec![0xD800];
        let mut ind: SqlLen = std::mem::size_of_val(&units[..]) as SqlLen;

        let mut p = param(SQL_C_WCHAR, units.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_WVARCHAR;
        p.column_size = 1;
        match unsafe { bound_param_to_value(&p) }.unwrap().0 {
            SqlType::NVarchar(Some(s), 1) => {
                assert_eq!(s.as_utf16_bytes(), Some(&[0x00, 0xD8][..]))
            }
            other => panic!("expected NVarchar(Some, 1), got {other:?}"),
        }

        let mut p = param(SQL_C_WCHAR, units.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_VARCHAR;
        p.column_size = 1;
        match unsafe { bound_param_to_value(&p) }.unwrap().0 {
            SqlType::Varchar(Some(s), 1) => assert_eq!(s.to_utf8_string(), "\u{FFFD}"),
            other => panic!("expected Varchar(Some, 1), got {other:?}"),
        }
    }

    // --- Cross-family: integer C type -> character SQL type -------------------

    /// Binds `v` under `c_type` and converts it to `sql_type`/`column_size`.
    fn convert_int_to_char(
        c_type: SqlSmallInt,
        v: i64,
        sql_type: SqlSmallInt,
        column_size: usize,
    ) -> Result<SqlType, ParamBuildError> {
        let mut buf = v;
        let mut ind: SqlLen = 0;
        let mut p = int_param(
            c_type,
            sql_type,
            &mut buf as *mut i64 as *mut c_void,
            &mut ind,
        );
        p.column_size = column_size;
        unsafe { bound_param_to_value(&p) }.map(|(value, _)| value)
    }

    /// Base 10, no padding, and a sign only when negative - the shape
    /// `_ltoa_s`/`BigintToChar` produce in `ConvertToChar`.
    #[test]
    fn an_integer_is_formatted_base_ten_into_a_character_target() {
        let cases: &[(i64, &str)] = &[(0, "0"), (7, "7"), (-42, "-42"), (1234567890, "1234567890")];
        for (v, expected) in cases {
            match convert_int_to_char(SQL_C_SBIGINT, *v, SQL_VARCHAR, 32).unwrap() {
                SqlType::Varchar(Some(s), 32) => assert_eq!(&s.to_utf8_string(), expected),
                other => panic!("{v}: expected Varchar(Some, 32), got {other:?}"),
            }
        }
    }

    /// The formatted digits are transcoded like any other character source, so
    /// a wide target receives UTF-16.
    #[test]
    fn an_integer_reaches_a_wide_character_target_as_utf16() {
        match convert_int_to_char(SQL_C_SLONG, -1, SQL_WVARCHAR, 8).unwrap() {
            SqlType::NVarchar(Some(s), 8) => {
                assert_eq!(s.as_utf16_bytes(), Some(&[b'-', 0, b'1', 0][..]))
            }
            other => panic!("expected NVarchar(Some, 8), got {other:?}"),
        }
    }

    /// `ParameterType` names the wire type, so one integer buffer reaches every
    /// character declaration.
    #[test]
    fn an_integer_reaches_every_character_declaration() {
        let cases: &[(SqlSmallInt, SqlType)] = &[
            (SQL_CHAR, SqlType::Char(None, 4)),
            (SQL_VARCHAR, SqlType::Varchar(None, 4)),
            (SQL_LONGVARCHAR, SqlType::VarcharMax(None)),
            (SQL_WCHAR, SqlType::NChar(None, 4)),
            (SQL_WVARCHAR, SqlType::NVarchar(None, 4)),
            (SQL_WLONGVARCHAR, SqlType::NVarcharMax(None)),
        ];
        for (sql_type, expected) in cases {
            let got = convert_int_to_char(SQL_C_SLONG, 12, *sql_type, 4).unwrap();
            assert_eq!(
                std::mem::discriminant(&got),
                std::mem::discriminant(expected),
                "{sql_type} produced {got:?}"
            );
        }
    }

    /// Digits are never blanks, so the trailing-blank exemption cannot absorb
    /// them: an over-long formatted value is `22001`, as msodbcsql reports after
    /// rewriting the inbound `01004` (`sqlcmisc.cpp:7429`).
    #[test]
    fn an_integer_too_wide_for_the_declared_length_is_22001() {
        assert_eq!(
            convert_int_to_char(SQL_C_SLONG, 12345, SQL_VARCHAR, 3),
            Err(ParamBuildError::StringTruncation)
        );
        // The sign counts against the length too.
        assert_eq!(
            convert_int_to_char(SQL_C_SLONG, -123, SQL_VARCHAR, 3),
            Err(ParamBuildError::StringTruncation)
        );
        assert!(convert_int_to_char(SQL_C_SLONG, 123, SQL_VARCHAR, 3).is_ok());
    }

    /// `SQL_C_UBIGINT` is widened unsigned, so a value past `i64::MAX` formats
    /// as itself rather than wrapping negative.
    #[test]
    fn an_unsigned_bigint_past_i64_max_formats_unsigned() {
        let mut buf = u64::MAX;
        let mut ind: SqlLen = 0;
        let mut p = int_param(
            SQL_C_UBIGINT,
            SQL_VARCHAR,
            &mut buf as *mut u64 as *mut c_void,
            &mut ind,
        );
        p.column_size = 32;
        match unsafe { bound_param_to_value(&p) }.unwrap().0 {
            SqlType::Varchar(Some(s), 32) => {
                assert_eq!(s.to_utf8_string(), "18446744073709551615")
            }
            other => panic!("expected Varchar(Some, 32), got {other:?}"),
        }
    }

    /// The tinyint sign rewrite is keyed on a `SQL_TINYINT` target, so a
    /// character target keeps `SQL_C_TINYINT` signed - the reading
    /// `ConvertToChar` uses when it loads the byte as `SCHAR`
    /// (`sqlccnvt.cpp:1673`).
    #[test]
    fn a_signed_tinyint_stays_signed_against_a_character_target() {
        let mut buf: i8 = -56;
        let mut ind: SqlLen = 0;
        let mut p = int_param(
            SQL_C_TINYINT,
            SQL_VARCHAR,
            &mut buf as *mut i8 as *mut c_void,
            &mut ind,
        );
        p.column_size = 8;
        match unsafe { bound_param_to_value(&p) }.unwrap().0 {
            SqlType::Varchar(Some(s), 8) => assert_eq!(s.to_utf8_string(), "-56"),
            other => panic!("expected Varchar(Some, 8), got {other:?}"),
        }
    }

    // --- Cross-family: character C type -> integer SQL type -------------------

    /// Exact integer literals round-trip through both character C types.
    #[test]
    fn an_integer_literal_reaches_an_integer_target() {
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
            let cases: &[(&str, SqlType)] = &[
                ("0", SqlType::Int(Some(0))),
                ("42", SqlType::Int(Some(42))),
                ("-42", SqlType::Int(Some(-42))),
                ("+42", SqlType::Int(Some(42))),
                // Blanks are padding on both ends (`sqlccnvt.cpp:7777`).
                ("   42   ", SqlType::Int(Some(42))),
            ];
            for (text, expected) in cases {
                assert_eq!(
                    convert_char(c_type, SQL_INTEGER, 0, text).unwrap(),
                    *expected,
                    "{c_type}: {text}"
                );
            }
        }
    }

    /// `ParameterType` names the wire type here too, and each target narrows
    /// independently.
    #[test]
    fn a_literal_reaches_every_integer_declaration() {
        let cases: &[(SqlSmallInt, SqlType)] = &[
            (SQL_TINYINT, SqlType::TinyInt(Some(12))),
            (SQL_SMALLINT, SqlType::SmallInt(Some(12))),
            (SQL_INTEGER, SqlType::Int(Some(12))),
            (SQL_BIGINT, SqlType::BigInt(Some(12))),
        ];
        for (sql_type, expected) in cases {
            assert_eq!(
                convert_char(SQL_C_CHAR, *sql_type, 0, "12").unwrap(),
                *expected
            );
        }
    }

    /// A dropped fraction is an *error* inbound, not the `01S07` warning the
    /// fetch direction reports: `ParamToSQLType` rewrites it to `IDS_22_001` for
    /// any non-2.x application (`sqlcfunc.cpp:3348`).
    #[test]
    fn a_dropped_fraction_is_22001_not_a_warning() {
        for text in ["12.7", "-0.5", "0.001"] {
            assert_eq!(
                convert_char(SQL_C_CHAR, SQL_INTEGER, 0, text),
                Err(ParamBuildError::StringTruncation),
                "{text}"
            );
        }
    }

    /// Only a *non-zero* dropped digit is truncation. msodbcsql flags the same
    /// way - `if (c != '0') Error = CVT_FRACT_TRUNC` (`sqlccnvt.cpp:7823`) - so
    /// a fraction that loses nothing converts cleanly.
    #[test]
    fn a_zero_fraction_loses_nothing_and_converts() {
        for text in ["12.", "12.0", "12.000", "-12.0"] {
            let expected = if text.starts_with('-') { -12 } else { 12 };
            assert_eq!(
                convert_char(SQL_C_CHAR, SQL_INTEGER, 0, text).unwrap(),
                SqlType::Int(Some(expected)),
                "{text}"
            );
        }
    }

    /// Narrowing runs before the fraction rewrite can fire, so `22003` wins when
    /// a value both overflows the target and carries a fraction.
    #[test]
    fn overflow_outranks_a_dropped_fraction() {
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_TINYINT, 0, "999.5"),
            Err(ParamBuildError::Value(ConvError::OutOfRange))
        );
    }

    /// A magnitude the target cannot hold is `22003`, including a negative into
    /// the one unsigned SQL integer.
    #[test]
    fn a_literal_outside_the_target_range_is_22003() {
        let cases: &[(SqlSmallInt, &str)] = &[
            (SQL_TINYINT, "256"),
            (SQL_TINYINT, "-1"),
            (SQL_SMALLINT, "32768"),
            (SQL_INTEGER, "2147483648"),
            (SQL_BIGINT, "9223372036854775808"),
            // Well-formed but past `i128`, so an overflow rather than a syntax
            // error (`sqlccnvt.cpp:7840` vs `:7809`).
            (SQL_BIGINT, "9999999999999999999999999999999999999999999"),
        ];
        for (sql_type, text) in cases {
            assert_eq!(
                convert_char(SQL_C_CHAR, *sql_type, 0, text),
                Err(ParamBuildError::Value(ConvError::OutOfRange)),
                "{sql_type}: {text}"
            );
        }
    }

    /// Anything that is not a numeric literal is `22018`. Only blanks are
    /// padding, so other whitespace and interior blanks are invalid too.
    #[test]
    fn a_non_numeric_literal_is_22018() {
        for text in [
            "abc", "", "   ", "1 2", "\t12", "12\n", "--1", "1.2.3", "0x1F", "NaN", "inf", "1e",
            "12abc", ".",
        ] {
            assert_eq!(
                convert_char(SQL_C_CHAR, SQL_INTEGER, 0, text),
                Err(ParamBuildError::Value(ConvError::InvalidCharacterValue)),
                "{text:?}"
            );
        }
    }

    /// Scientific notation is accepted, matching msodbcsql's split to
    /// `CharToDouble` once it spots an `e`/`E` (`sqlccnvt.cpp:5088`).
    #[test]
    fn scientific_notation_is_accepted() {
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_INTEGER, 0, "1e3").unwrap(),
            SqlType::Int(Some(1000))
        );
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_INTEGER, 0, "-1.5E2").unwrap(),
            SqlType::Int(Some(-150))
        );
        // An exponent past the `f64` range parses as infinity, which is an
        // overflow rather than a syntax error.
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_BIGINT, 0, "1e400"),
            Err(ParamBuildError::Value(ConvError::OutOfRange))
        );
    }

    /// The wide arm decodes UTF-16 rather than narrowing through the ANSI code
    /// page as msodbcsql does, which agrees on every string that parses.
    #[test]
    fn a_wide_literal_parses_through_utf16() {
        assert_eq!(
            convert_char(SQL_C_WCHAR, SQL_BIGINT, 0, "-9223372036854775808").unwrap(),
            SqlType::BigInt(Some(i64::MIN))
        );
        assert_eq!(
            convert_char(SQL_C_WCHAR, SQL_INTEGER, 0, "١٢"),
            Err(ParamBuildError::Value(ConvError::InvalidCharacterValue))
        );
    }

    /// `ColumnSize` describes a character declaration, so it has no say over an
    /// integer target - the value is bounded by the target's range instead.
    #[test]
    fn column_size_does_not_bound_an_integer_target() {
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_INTEGER, 1, "1234567").unwrap(),
            SqlType::Int(Some(1234567))
        );
    }

    /// The parse is locale-independent in both drivers. msodbcsql keeps an
    /// entire NLS suite for numeric parameters
    /// (`testsrc/.../ODBCNLS/src/TCSQLBindParamNonChar.cpp`), which still binds
    /// the *invariant* spelling: `CharToBigint` accepts ASCII digits and a
    /// leading sign only, so a grouped or comma-decimal literal is `CVT_ERROR`
    /// however the thread locale is set.
    #[test]
    fn locale_formatted_numbers_are_rejected() {
        for text in ["1,234", "1,5", "1 234", "1.234,5", "\u{a0}12"] {
            assert_eq!(
                convert_char(SQL_C_CHAR, SQL_INTEGER, 0, text),
                Err(ParamBuildError::Value(ConvError::InvalidCharacterValue)),
                "{text:?}"
            );
        }
    }

    /// A numeric literal reaches the parser through the same indicator rules as
    /// any other character buffer, `SQL_NTS` included - which is how an
    /// application most often binds one.
    #[test]
    fn an_nts_literal_reaches_an_integer_target() {
        let mut buf: Vec<u8> = b"-42\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_INTEGER;
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap().0,
            SqlType::Int(Some(-42))
        );

        // An explicit length that counts the terminator still parses:
        // `CharToBigint` loops `while (len < srclen && charstr[len] != '\0')`
        // (`sqlccnvt.cpp:7800`), so the NUL ends the number rather than
        // invalidating it. Passing `strlen + 1` is a common enough application
        // slip to be worth matching.
        let mut ind: SqlLen = 4;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_INTEGER;
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap().0,
            SqlType::Int(Some(-42))
        );
    }

    /// A leading NUL leaves no number. msodbcsql's `CharToBigint` loop exits
    /// immediately and yields 0 with `CVT_NO_ERROR`; this rejects instead, which
    /// is the same answer msodbcsql gives for the identical buffer under
    /// `SQL_NTS`. Pinned because it is the one NUL position where the two differ.
    #[test]
    fn a_leading_nul_is_not_a_number() {
        let mut buf: Vec<u8> = vec![0, b'1', b'2'];
        let mut ind: SqlLen = 3;
        let mut p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        p.sql_type = SQL_INTEGER;
        assert_eq!(
            unsafe { bound_param_to_value(&p) },
            Err(ParamBuildError::Value(ConvError::InvalidCharacterValue))
        );
    }

    /// An unbounded `ColumnSize` selects `varchar(max)` for a formatted integer
    /// exactly as it does for a character source, so the digits are never
    /// length-checked.
    #[test]
    fn an_integer_reaches_the_max_types_when_column_size_is_unbounded() {
        match convert_int_to_char(SQL_C_SLONG, 12345, SQL_VARCHAR, 0).unwrap() {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "12345"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
        match convert_int_to_char(SQL_C_SLONG, 12345, SQL_WVARCHAR, 0).unwrap() {
            SqlType::NVarcharMax(Some(_)) => {}
            other => panic!("expected NVarcharMax(Some), got {other:?}"),
        }
    }

    /// A fraction is reported however long the literal. `parse_decimal_literal`
    /// gives up past an exact `i128` mantissa, and routing that to `f64` would
    /// round the fraction away silently - `CharToBigint` flags any non-zero
    /// digit past the scale regardless of length (`sqlccnvt.cpp:7823`).
    #[test]
    fn a_fraction_past_the_exact_mantissa_is_still_22001() {
        let wide = format!("1.{}1", "0".repeat(42));
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_INTEGER, 0, &wide),
            Err(ParamBuildError::StringTruncation),
            "{wide}"
        );

        // All-zero past the mantissa drops nothing, so it still converts.
        let zeros = format!("7.{}", "0".repeat(42));
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_INTEGER, 0, &zeros).unwrap(),
            SqlType::Int(Some(7))
        );

        // An integer part too wide for any target stays `22003`, matching
        // `CVT_PREC` from the accumulator.
        let huge = format!("{}.5", "9".repeat(41));
        assert_eq!(
            convert_char(SQL_C_CHAR, SQL_BIGINT, 0, &huge),
            Err(ParamBuildError::Value(ConvError::OutOfRange))
        );
    }

    /// Only the `max` types are unbounded. `text`/`ntext` have no declared
    /// length but are still bounded by `ColumnSize`, as msodbcsql bounds them
    /// (`sqlcfunc.cpp:2898`).
    #[test]
    fn only_the_max_types_are_unbounded() {
        use crate::api::type_rules::parameter_column_size_is_valid;

        let long = "x".repeat(9000);
        for (c_type, sql_type) in [(SQL_C_CHAR, SQL_VARCHAR), (SQL_C_WCHAR, SQL_WVARCHAR)] {
            assert!(
                convert_char(c_type, sql_type, 0, &long).is_ok(),
                "{c_type} -> {sql_type} truncated a max target"
            );
        }
        for (c_type, sql_type) in [
            (SQL_C_CHAR, SQL_LONGVARCHAR),
            (SQL_C_CHAR, SQL_WLONGVARCHAR),
            (SQL_C_WCHAR, SQL_LONGVARCHAR),
            (SQL_C_WCHAR, SQL_WLONGVARCHAR),
        ] {
            assert!(convert_char(c_type, sql_type, 9000, &long).is_ok());
            assert_eq!(
                convert_char(c_type, sql_type, 3, "abcd").unwrap_err(),
                ParamBuildError::StringTruncation,
                "{c_type} -> {sql_type} ignored ColumnSize"
            );
        }

        // 0 is not the `max` spelling for these: it sets the bound to zero and
        // rejects every non-empty value, so the bind gate refuses it with HY104
        // first - as msodbcsql's `CheckSqlPrec` does (`sqlcdesc.cpp:11805`).
        for sql_type in [SQL_LONGVARCHAR, SQL_WLONGVARCHAR] {
            assert!(!parameter_column_size_is_valid(sql_type, 0));
            assert_eq!(
                convert_char(SQL_C_CHAR, sql_type, 0, "a").unwrap_err(),
                ParamBuildError::StringTruncation,
                "sql_type {sql_type}"
            );
        }
    }

    /// `sql_family` and the bind-time matrix hold the same type knowledge in two
    /// places, so pin them together. The matrix no longer partitions by family -
    /// a character buffer reaches the integer types and an integer buffer the
    /// character ones - so what it can still settle is *reachability*: a
    /// `sql_type` has a family exactly when some C type can reach it. The family
    /// assignment itself is pinned by the explicit lists that follow.
    #[test]
    fn sql_family_agrees_with_the_conversion_matrix() {
        for sql_type in -160..=120 {
            let reachable = [SQL_C_CHAR, SQL_C_SLONG, SQL_C_BINARY]
                .iter()
                .any(|c_type| is_supported_conversion(*c_type, sql_type));
            assert_eq!(
                sql_family(sql_type).is_some(),
                reachable,
                "sql_type {sql_type}"
            );
        }

        let families: &[(SqlFamily, &[SqlSmallInt])] = &[
            (
                SqlFamily::Character,
                &[
                    SQL_CHAR,
                    SQL_VARCHAR,
                    SQL_LONGVARCHAR,
                    SQL_WCHAR,
                    SQL_WVARCHAR,
                    SQL_WLONGVARCHAR,
                ],
            ),
            (
                SqlFamily::Integer,
                &[SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT],
            ),
            (
                SqlFamily::Binary,
                &[SQL_BINARY, SQL_VARBINARY, SQL_LONGVARBINARY],
            ),
        ];
        for (family, sql_types) in families {
            for sql_type in *sql_types {
                assert_eq!(sql_family(*sql_type), Some(*family), "sql_type {sql_type}");
            }
        }
    }

    #[test]
    fn null_indicator_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_VARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarcharMax(None)));
    }

    #[test]
    fn unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = 4;
        let mut val: f32 = 1.5;
        let p = param(SQL_C_FLOAT, &mut val as *mut f32 as *mut c_void, &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::UnsupportedCType(SQL_C_FLOAT));
    }

    #[test]
    fn data_at_exec_is_rejected() {
        let mut ind: SqlLen = SQL_DATA_AT_EXEC;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::DataAtExecNotStaged);
    }

    /// `SQL_LEN_DATA_AT_EXEC(n)` encodes as a large negative indicator, which
    /// would otherwise be caught by the invalid-length check below it and
    /// misreported as a bad buffer length.
    #[test]
    fn data_at_exec_with_declared_length_is_rejected() {
        let mut ind: SqlLen = SQL_LEN_DATA_AT_EXEC_OFFSET - 16;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::DataAtExecNotStaged);
    }

    #[test]
    fn invalid_indicator_is_rejected() {
        let mut ind: SqlLen = SQL_NO_TOTAL;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::InvalidLength(SQL_NO_TOTAL));
    }

    /// `SQL_DEFAULT_PARAM` is only legal for a canonical procedure call, which
    /// this driver does not support, so it is 07S01 rather than "not yet
    /// implemented" (msodbcsql `sqlccmd.cpp` -> IDS_07_S01).
    #[test]
    fn default_param_indicator_is_invalid_use_not_unimplemented() {
        let mut ind: SqlLen = SQL_DEFAULT_PARAM;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::InvalidUseOfDefaultParam);
        assert_eq!(err.diag().state, *b"07S01");
    }

    #[test]
    fn null_indicator_wchar_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = param(SQL_C_WCHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_WVARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarcharMax(None)));
    }

    #[test]
    fn null_indicator_unsupported_sql_type_is_rejected() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_SS_UDT;
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::UnsupportedSqlType(SQL_SS_UDT));
    }

    /// A NULL is still declared with its SQL type, so a fixed-length one needs a
    /// real length: `char(0)` is not legal T-SQL. `SQLBindParameter` rejects the
    /// `ColumnSize` before this point, so reaching here means the two checks
    /// disagree.
    #[test]
    fn explicit_char_null_with_zero_column_size_is_rejected() {
        for (c_type, sql_type) in [(SQL_C_CHAR, SQL_CHAR), (SQL_C_WCHAR, SQL_WCHAR)] {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = param(c_type, std::ptr::null_mut(), &mut ind);
            p.sql_type = sql_type;
            let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
            assert_eq!(
                err,
                ParamBuildError::InvalidParameterSize(0),
                "sql_type {sql_type}"
            );
            assert_eq!(err.diag().state, *b"HY104");
        }
    }

    /// The variable-length types read the same 0 as the `max` spelling.
    #[test]
    fn explicit_varchar_null_with_zero_column_size_is_max() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_VARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarcharMax(None)));

        let mut p = param(SQL_C_WCHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_WVARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarcharMax(None)));
    }

    /// `SQLBindParameter` screens `ColumnSize` with `parameter_column_size_is_valid`
    /// and the declaration is built here from the same value, so the two encode
    /// the same limits in two places. Anything the bind gate accepts must be
    /// declarable, or the application gets `HY104` from a call it was told had
    /// succeeded.
    ///
    /// Only that direction is asserted. The declaration is deliberately laxer:
    /// an oversized variable-length `ColumnSize` widens to `max` here
    /// (`variable_length`), which the bind gate rejects first, matching
    /// msodbcsql's `CheckSqlPrec`.
    #[test]
    fn every_bind_accepted_column_size_is_declarable() {
        use crate::api::type_rules::parameter_column_size_is_valid;

        for sql_type in [
            SQL_CHAR,
            SQL_WCHAR,
            SQL_BINARY,
            SQL_VARCHAR,
            SQL_WVARCHAR,
            SQL_VARBINARY,
            SQL_LONGVARCHAR,
            SQL_WLONGVARCHAR,
            SQL_LONGVARBINARY,
            SQL_SS_XML,
            SQL_DECIMAL,
            SQL_NUMERIC,
        ] {
            for column_size in [0, 1, 37, 38, 39, 4000, 4001, 8000, 8001] {
                if !parameter_column_size_is_valid(sql_type, column_size) {
                    continue;
                }
                assert!(
                    typed_null(sql_type, column_size, 0).is_ok(),
                    "sql_type {sql_type}: bind accepts ColumnSize {column_size} \
                     but the declaration cannot be built"
                );
            }
        }
    }

    #[test]
    fn default_null_uses_described_sql_type() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = default_param(SQL_INTEGER, &mut ind);
        p.column_size = 10;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::Int(None)));

        p.sql_type = SQL_WVARCHAR;
        p.column_size = 40;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarchar(None, 40)));
    }

    /// A defaulted bind of a character SQL type really can describe its buffer
    /// with a character C type, so it reads normally.
    ///
    /// The converse - `decimal`, `numeric`, `sql_variant` and `xml` borrowing
    /// `SQL_C_CHAR`/`SQL_C_WCHAR` and being read as text - is now rejected at
    /// bind by the conversion matrix, and is covered by
    /// `default_bind_rejects_sql_types_whose_default_c_type_is_character`.
    #[test]
    fn default_non_null_reads_character_sql_types() {
        let mut buf: Vec<u8> = b"hello\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let mut p = default_param(SQL_VARCHAR, &mut ind);
        p.parameter_value_ptr = buf.as_mut_ptr() as *mut c_void;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    /// The typed NULL and the precision/scale metadata must be produced
    /// together, so the declaration `RpcParameter` renders cannot disagree with
    /// the value it serializes. (That the metadata then drives both is covered
    /// by `type_metadata_drives_declaration_and_wire_metadata` in `mssql-tds`.)
    #[test]
    fn default_null_pairs_value_with_metadata() {
        let decimal = |precision, scale| {
            Some(RpcTypeMetadata {
                precision: Some(precision),
                scale: Some(scale),
            })
        };
        let temporal = |scale| {
            Some(RpcTypeMetadata {
                precision: None,
                scale: Some(scale),
            })
        };
        let cases: &[(
            SqlSmallInt,
            usize,
            SqlSmallInt,
            SqlType,
            Option<RpcTypeMetadata>,
        )] = &[
            (SQL_DECIMAL, 12, 3, SqlType::Decimal(None), decimal(12, 3)),
            (SQL_NUMERIC, 38, 0, SqlType::Numeric(None), decimal(38, 0)),
            (SQL_SS_TIME2, 16, 4, SqlType::Time(None), temporal(4)),
            (
                SQL_TYPE_TIMESTAMP,
                27,
                7,
                SqlType::DateTime2(None),
                temporal(7),
            ),
            (
                SQL_SS_TIMESTAMPOFFSET,
                34,
                7,
                SqlType::DateTimeOffset(None),
                temporal(7),
            ),
            (SQL_INTEGER, 10, 0, SqlType::Int(None), None),
            (SQL_CHAR, 10, 0, SqlType::Char(None, 10), None),
            (SQL_WVARCHAR, 40, 0, SqlType::NVarchar(None, 40), None),
            // An oversized `ColumnSize` and `i32::MAX` both mean `max`.
            (
                SQL_WVARCHAR,
                OVERSIZED_COLUMN_SIZE,
                0,
                SqlType::NVarcharMax(None),
                None,
            ),
            (SQL_VARCHAR, 9000, 0, SqlType::VarcharMax(None), None),
            (
                SQL_VARBINARY,
                OVERSIZED_COLUMN_SIZE,
                0,
                SqlType::VarBinaryMax(None),
                None,
            ),
        ];
        for (sql_type, column_size, decimal_digits, expected_value, expected_metadata) in cases {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = default_param(*sql_type, &mut ind);
            p.column_size = *column_size;
            p.decimal_digits = *decimal_digits;
            let (value, metadata) = unsafe { bound_param_to_value(&p) }
                .unwrap_or_else(|e| panic!("conversion failed for {sql_type}: {e:?}"));
            assert_eq!(&value, expected_value, "case: sql_type {sql_type}");
            assert_eq!(&metadata, expected_metadata, "case: sql_type {sql_type}");
        }
    }

    /// A `ColumnSize`/`DecimalDigits` that has no legal T-SQL spelling is
    /// rejected here rather than sent as a malformed declaration.
    #[test]
    fn default_null_rejects_undeclarable_metadata() {
        let cases: &[(SqlSmallInt, usize, SqlSmallInt, ParamBuildError)] = &[
            (SQL_DECIMAL, 0, 0, ParamBuildError::InvalidParameterSize(0)),
            (
                SQL_DECIMAL,
                39,
                0,
                ParamBuildError::InvalidParameterSize(39),
            ),
            // Scale may not exceed precision.
            (SQL_NUMERIC, 5, 6, ParamBuildError::InvalidDecimalDigits(6)),
            (SQL_CHAR, 0, 0, ParamBuildError::InvalidParameterSize(0)),
            (
                SQL_WCHAR,
                4001,
                0,
                ParamBuildError::InvalidParameterSize(4001),
            ),
            (SQL_BINARY, 0, 0, ParamBuildError::InvalidParameterSize(0)),
            (
                SQL_TYPE_TIMESTAMP,
                27,
                8,
                ParamBuildError::InvalidDecimalDigits(8),
            ),
            (
                SQL_SS_TIME2,
                16,
                -1,
                ParamBuildError::InvalidDecimalDigits(-1),
            ),
            (
                SQL_SS_UDT,
                0,
                0,
                ParamBuildError::UnsupportedSqlType(SQL_SS_UDT),
            ),
        ];
        for &(sql_type, column_size, decimal_digits, expected) in cases {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = default_param(sql_type, &mut ind);
            p.column_size = column_size;
            p.decimal_digits = decimal_digits;
            let err = unsafe { bound_param_to_value(&p) }
                .expect_err(&format!("expected rejection for sql_type {sql_type}"));
            assert_eq!(err, expected, "case: sql_type {sql_type}");
        }
    }

    /// A vector's `ColumnSize` is the client buffer size: header + 4 bytes per
    /// dimension, regardless of the server-side base type.
    #[test]
    fn vector_metadata_round_trips_dimensions() {
        let header = std::mem::size_of::<SqlSsVectorLayout>();
        assert_eq!(
            vector_metadata(header + 3 * SQL_SS_VECTOR_ELEMENT_SIZE, 0).unwrap(),
            (3, VectorBaseType::Float32)
        );
        assert_eq!(
            vector_metadata(header + 3 * SQL_SS_VECTOR_ELEMENT_SIZE, 1).unwrap(),
            (3, VectorBaseType::Float16)
        );
        // Too small for the header, and a payload that is not a whole number of
        // elements, are both rejected.
        assert_eq!(
            vector_metadata(1, 0).unwrap_err(),
            ParamBuildError::InvalidParameterSize(1)
        );
        assert_eq!(
            vector_metadata(header + 3, 0).unwrap_err(),
            ParamBuildError::InvalidParameterSize(header + 3)
        );
        assert_eq!(
            vector_metadata(header, 2).unwrap_err(),
            ParamBuildError::InvalidDecimalDigits(2)
        );
    }
}
