// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLColAttributeW.
//!
//! Field values come from the same `ColumnMetadata` mapping `SQLDescribeColW`
//! uses, so the two APIs cannot report different types for the same column.

use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;
use tracing::{debug, error};

use crate::api::describe_col::{column_size, decimal_digits, odbc_sql_type};
use crate::api::odbc_types::{
    SQL_ATTR_READWRITE_UNKNOWN, SQL_BIGINT, SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DATE,
    SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID, SQL_C_NUMERIC, SQL_C_SBIGINT, SQL_C_SLONG,
    SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT, SQL_C_TIMESTAMP, SQL_C_UTINYINT,
    SQL_C_WCHAR, SQL_CA_SS_VARIANT_TYPE, SQL_CODE_TIMESTAMP, SQL_DATETIME, SQL_DECIMAL,
    SQL_DESC_AUTO_UNIQUE_VALUE, SQL_DESC_BASE_COLUMN_NAME, SQL_DESC_CASE_SENSITIVE,
    SQL_DESC_CONCISE_TYPE, SQL_DESC_COUNT, SQL_DESC_DATETIME_INTERVAL_CODE, SQL_DESC_DISPLAY_SIZE,
    SQL_DESC_FIXED_PREC_SCALE, SQL_DESC_LABEL, SQL_DESC_LENGTH, SQL_DESC_NAME, SQL_DESC_NULLABLE,
    SQL_DESC_NUM_PREC_RADIX, SQL_DESC_OCTET_LENGTH, SQL_DESC_PRECISION, SQL_DESC_SCALE,
    SQL_DESC_SEARCHABLE, SQL_DESC_TYPE, SQL_DESC_TYPE_NAME, SQL_DESC_UNNAMED, SQL_DESC_UNSIGNED,
    SQL_DESC_UPDATABLE, SQL_DOUBLE, SQL_ERROR, SQL_FLOAT, SQL_INTEGER, SQL_INVALID_HANDLE,
    SQL_NAMED, SQL_NO_NULLS, SQL_NULLABLE, SQL_NUMERIC, SQL_PRED_BASIC, SQL_PRED_CHAR,
    SQL_PRED_NONE, SQL_PRED_SEARCHABLE, SQL_REAL, SQL_SMALLINT, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SQL_UNNAMED, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_DESCRIPTOR_FIELD, ERR_INVALID_DESCRIPTOR_INDEX,
    ERR_NOT_VARIANT_COLUMN, WARN_STRING_TRUNCATION, post_diag,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_EXEC_CONTEXT;
use crate::handles::{HandleType, OdbcVersion, StmtHandle, handle_from_raw};

/// Gets a descriptor field for a result-set column.
///
/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
/// `character_attribute_ptr`, when non-null, must be writable for
/// `buffer_length` bytes. `string_length_ptr` and `numeric_attribute_ptr`, when
/// non-null, must each be writable for one value of their pointed-to type.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_col_attribute_w(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    field_identifier: SqlUSmallInt,
    character_attribute_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    numeric_attribute_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number,
        field_identifier,
        ?character_attribute_ptr,
        buffer_length,
        ?string_length_ptr,
        ?numeric_attribute_ptr,
        "SQLColAttributeW called",
    );

    crate::ffi_entry!("SQLColAttributeW", unsafe {
        sql_col_attribute_w_impl(
            statement_handle,
            column_number,
            field_identifier,
            character_attribute_ptr,
            buffer_length,
            string_length_ptr,
            numeric_attribute_ptr,
        )
    })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
/// `character_attribute_ptr`, when non-null, must be writable for
/// `buffer_length` bytes. `string_length_ptr` and `numeric_attribute_ptr`, when
/// non-null, must each be writable for one value of their pointed-to type.
#[allow(clippy::too_many_arguments)]
unsafe fn sql_col_attribute_w_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    field_identifier: SqlUSmallInt,
    character_attribute_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    numeric_attribute_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLColAttributeW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLColAttributeW: handle is not a STMT"
    );

    sql_col_attribute_w_safe(
        stmt,
        column_number,
        field_identifier,
        character_attribute_ptr,
        buffer_length,
        string_length_ptr,
        numeric_attribute_ptr,
    )
}

/// Which output parameter a field identifier writes to.
enum Attr {
    Numeric(SqlLen),
    Text(String),
}

#[allow(clippy::too_many_arguments)]
fn sql_col_attribute_w_safe(
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    field_identifier: SqlUSmallInt,
    character_attribute_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    numeric_attribute_ptr: *mut SqlLen,
) -> SqlReturn {
    // The declared ODBC version selects the temporal sql_variant C types. Read
    // it before the stmt lock to preserve parent-before-child lock ordering.
    let variant_uses_3_80_types = if field_identifier == SQL_CA_SS_VARIANT_TYPE {
        let env = stmt.parent_dbc().parent_env();
        let Ok(env_state) = env.inner.lock() else {
            error!("SQLColAttributeW: env mutex poisoned");
            return SQL_ERROR;
        };
        uses_3_80_variant_types(env_state.odbc_version)
    } else {
        false
    };

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLColAttributeW: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    if !stmt_state.has_state(STMT_STATE_EXEC_CONTEXT) {
        post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    }

    // SQL_DESC_COUNT describes the result set, not a column, so it is answered
    // before the column number is validated.
    if field_identifier == SQL_DESC_COUNT {
        let count = SqlLen::try_from(stmt_state.column_metadata.len()).unwrap_or(SqlLen::MAX);
        unsafe { write_if_some(numeric_attribute_ptr, count) };
        return SQL_SUCCESS;
    }

    if column_number == 0 || column_number as usize > stmt_state.column_metadata.len() {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // The underlying type of a `sql_variant` is a property of the value, not the
    // column, so it comes from the row that was read rather than the metadata.
    if field_identifier == SQL_CA_SS_VARIANT_TYPE {
        let is_variant = stmt_state.column_metadata[(column_number - 1) as usize].data_type
            == TdsDataType::SsVariant;
        if !is_variant {
            post_diag(&mut stmt_state, ERR_NOT_VARIANT_COLUMN);
            return SQL_ERROR;
        }
        // The base type belongs to the value that was probed, so it only answers
        // for the column it came from.
        let base = stmt_state
            .last_variant_base
            .filter(|(col, _)| *col == column_number as usize)
            .map(|(_, base)| base);
        let Some(base) = base else {
            // Callers probe the column with SQLGetData first; that read is what
            // supplies the base type.
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        };
        unsafe {
            write_if_some(
                numeric_attribute_ptr,
                SqlLen::from(variant_c_type(base, variant_uses_3_80_types)),
            )
        };
        return SQL_SUCCESS;
    }

    let meta = &stmt_state.column_metadata[(column_number - 1) as usize];
    let Some(attr) = column_attribute(meta, field_identifier) else {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    };

    match attr {
        Attr::Numeric(v) => {
            unsafe { write_if_some(numeric_attribute_ptr, v) };
            SQL_SUCCESS
        }
        Attr::Text(s) => {
            let utf16: Vec<u16> = s.encode_utf16().collect();
            // StringLengthPtr is in bytes for the wide entry point, and excludes
            // the terminator.
            let byte_len = SqlSmallInt::try_from(utf16.len() * std::mem::size_of::<SqlWChar>())
                .unwrap_or(SqlSmallInt::MAX);
            unsafe { write_if_some(string_length_ptr, byte_len) };

            let buf_elements = if buffer_length > 0 {
                (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
            } else {
                0
            };
            let truncated = unsafe {
                copy_with_nul(
                    character_attribute_ptr as *mut SqlWChar,
                    buf_elements,
                    &utf16,
                )
            };
            if truncated {
                post_diag(&mut stmt_state, WARN_STRING_TRUNCATION);
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
    }
}

/// Maps a field identifier to its value, or `None` when the field is not one
/// this driver reports.
fn column_attribute(meta: &ColumnMetadata, field_identifier: SqlUSmallInt) -> Option<Attr> {
    let attr = match field_identifier {
        SQL_DESC_CONCISE_TYPE => Attr::Numeric(SqlLen::from(concise_type(meta))),
        SQL_DESC_TYPE => Attr::Numeric(SqlLen::from(verbose_type(meta))),
        SQL_DESC_DATETIME_INTERVAL_CODE => Attr::Numeric(datetime_interval_code(meta)),
        SQL_DESC_LENGTH => Attr::Numeric(desc_length(meta)),
        SQL_DESC_DISPLAY_SIZE => Attr::Numeric(display_size(meta)),
        SQL_DESC_OCTET_LENGTH => Attr::Numeric(octet_length(meta)),
        SQL_DESC_PRECISION => Attr::Numeric(SqlLen::from(precision(meta))),
        SQL_DESC_SCALE => Attr::Numeric(SqlLen::from(decimal_digits(meta))),
        SQL_DESC_NULLABLE => Attr::Numeric(SqlLen::from(if meta.is_nullable() {
            SQL_NULLABLE
        } else {
            SQL_NO_NULLS
        })),
        // The boolean attributes are SQL_TRUE (1) / SQL_FALSE (0), which is what
        // `bool` converts to.
        SQL_DESC_UNSIGNED => Attr::Numeric(SqlLen::from(is_unsigned(meta))),
        SQL_DESC_CASE_SENSITIVE => Attr::Numeric(SqlLen::from(meta.is_case_sensitive())),
        SQL_DESC_FIXED_PREC_SCALE => Attr::Numeric(SqlLen::from(matches!(
            meta.data_type,
            TdsDataType::Money | TdsDataType::Money4 | TdsDataType::MoneyN
        ))),
        SQL_DESC_NUM_PREC_RADIX => Attr::Numeric(num_prec_radix(meta)),
        SQL_DESC_UNNAMED => Attr::Numeric(if meta.column_name.is_empty() {
            SQL_UNNAMED
        } else {
            SQL_NAMED
        }),
        // COLMETADATA carries no updatability flag, so the result set is not
        // known to be updatable either way.
        SQL_DESC_UPDATABLE => Attr::Numeric(SQL_ATTR_READWRITE_UNKNOWN),
        SQL_DESC_AUTO_UNIQUE_VALUE => Attr::Numeric(SqlLen::from(meta.is_identity())),
        SQL_DESC_SEARCHABLE => Attr::Numeric(searchable(meta)),
        SQL_DESC_NAME | SQL_DESC_LABEL => Attr::Text(meta.column_name.clone()),
        // `column_name` is the result-set label, which for `SELECT c AS alias`
        // is the alias. COLMETADATA carries no base-column name unless the query
        // is FOR BROWSE, so ODBC's "provenance unknown" answer is the right one.
        SQL_DESC_BASE_COLUMN_NAME => Attr::Text(String::new()),
        SQL_DESC_TYPE_NAME => Attr::Text(type_name(meta).to_string()),
        _ => return None,
    };
    Some(attr)
}

/// True for the types SQL Server reports as `datetime`/`smalldatetime`/
/// `datetime2`, which are the only ones ODBC folds into the verbose
/// `SQL_DATETIME` type. `date` is deliberately excluded: msodbcsql remaps it to
/// `SQL_TYPE_DATE` before the verbose fold, so it reports the concise type for
/// both fields, and `time`/`datetimeoffset` use SQL Server-specific types that
/// are outside the ODBC datetime range.
fn is_odbc_timestamp(meta: &ColumnMetadata) -> bool {
    match meta.data_type {
        TdsDataType::DateTime | TdsDataType::DateTim4 | TdsDataType::DateTime2N => true,
        TdsDataType::DateTimeN => matches!(meta.type_info.length, 4 | 8),
        _ => false,
    }
}

fn concise_type(meta: &ColumnMetadata) -> SqlSmallInt {
    odbc_sql_type(meta)
}

/// `SQL_DESC_TYPE` is the verbose type: the timestamp family collapses to
/// `SQL_DATETIME`, with the member identified by `SQL_DESC_DATETIME_INTERVAL_CODE`.
fn verbose_type(meta: &ColumnMetadata) -> SqlSmallInt {
    if is_odbc_timestamp(meta) {
        SQL_DATETIME
    } else {
        odbc_sql_type(meta)
    }
}

/// Only meaningful when the verbose type is `SQL_DATETIME`; zero otherwise.
fn datetime_interval_code(meta: &ColumnMetadata) -> SqlLen {
    if is_odbc_timestamp(meta) {
        SQL_CODE_TIMESTAMP
    } else {
        0
    }
}

/// `SQL_DESC_LENGTH`: the column size, except that the approximate numerics
/// report binary precision.
pub(super) fn desc_length(meta: &ColumnMetadata) -> SqlLen {
    if let Some(binary) = binary_precision(meta) {
        return SqlLen::from(binary);
    }
    SqlLen::try_from(column_size(meta)).unwrap_or(SqlLen::MAX)
}

/// `real` and `float` report binary precision (24 and 53) rather than the
/// decimal precision `SQLDescribeCol` reports as the column size.
fn binary_precision(meta: &ColumnMetadata) -> Option<SqlSmallInt> {
    match meta.data_type {
        TdsDataType::Flt4 => Some(24),
        TdsDataType::Flt8 => Some(53),
        TdsDataType::FltN => match meta.type_info.length {
            4 => Some(24),
            8 => Some(53),
            _ => None,
        },
        _ => None,
    }
}

/// `SQL_DESC_DISPLAY_SIZE`: the character count needed to render the value.
///
/// This is not the column size: an `int` needs 11 characters (sign plus ten
/// digits) for a column size of 10, a GUID needs 36, and binary renders as two
/// hex characters per byte.
fn display_size(meta: &ColumnMetadata) -> SqlLen {
    // `*(max)`, xml, json, and opaque UDTs have no fixed character display size.
    if meta.is_plp() {
        return 0;
    }
    let size: u64 = match meta.data_type {
        TdsDataType::Bit | TdsDataType::BitN => 1,
        TdsDataType::Int1 => 3,
        TdsDataType::Int2 => 6,
        TdsDataType::Int4 => 11,
        TdsDataType::Int8 => 20,
        TdsDataType::IntN => match meta.type_info.length {
            1 => 3,
            2 => 6,
            4 => 11,
            8 => 20,
            _ => 0,
        },
        // Sign, leading digit, decimal point, exponent.
        TdsDataType::Flt4 => 14,
        TdsDataType::Flt8 => 24,
        TdsDataType::FltN => match meta.type_info.length {
            4 => 14,
            8 => 24,
            _ => 0,
        },
        // Precision plus the sign and the decimal point.
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN => u64::from(meta.get_precision().unwrap_or(0)) + 2,
        TdsDataType::Money => 21,
        TdsDataType::Money4 => 12,
        TdsDataType::MoneyN => match meta.type_info.length {
            8 => 21,
            4 => 12,
            _ => 0,
        },
        // 32 hex digits plus 4 dashes.
        TdsDataType::Guid => 36,
        // The temporal display widths are the rendered literal lengths, which is
        // exactly what the column size already reports.
        TdsDataType::DateN
        | TdsDataType::TimeN
        | TdsDataType::DateTime
        | TdsDataType::DateTime2N
        | TdsDataType::DateTimeOffsetN => column_size(meta),
        // smalldatetime renders seconds even though it does not store them.
        TdsDataType::DateTim4 => 19,
        TdsDataType::DateTimeN => match meta.type_info.length {
            8 => 23,
            4 => 19,
            _ => 0,
        },
        // Two hex characters per byte.
        TdsDataType::Binary
        | TdsDataType::BigBinary
        | TdsDataType::VarBinary
        | TdsDataType::BigVarBinary
        | TdsDataType::Image => 2 * meta.type_info.length as u64,
        // A variant's display size depends on the value it carries; msodbcsql
        // reports the widest non-max character column instead.
        TdsDataType::SsVariant => 8000,
        // Character types render one character per character, which for the
        // national types is half the wire byte count.
        _ => column_size(meta),
    };
    SqlLen::try_from(size).unwrap_or(SqlLen::MAX)
}

/// `SQL_DESC_OCTET_LENGTH`: the size in bytes of the value's ODBC *transfer*
/// representation, which for the temporal types is the C struct the driver
/// hands back, not the TDS payload width.
pub(super) fn octet_length(meta: &ColumnMetadata) -> SqlLen {
    // A bounded UDT is still PLP; use its byte limit for binary transfer while
    // display_size remains zero because the opaque value has no text rendering.
    if meta.data_type == TdsDataType::Udt {
        return desc_length(meta);
    }
    if meta.is_plp() {
        return 0;
    }
    match meta.data_type {
        // SQL_DATE_STRUCT.
        TdsDataType::DateN => 6,
        // SQL_SS_TIME2_STRUCT.
        TdsDataType::TimeN => 12,
        // SQL_TIMESTAMP_STRUCT.
        TdsDataType::DateTime
        | TdsDataType::DateTim4
        | TdsDataType::DateTimeN
        | TdsDataType::DateTime2N => 16,
        // SQL_SS_TIMESTAMPOFFSET_STRUCT.
        TdsDataType::DateTimeOffsetN => 20,
        // The exact numerics transfer as characters, so the octet length is the
        // rendered width.
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => display_size(meta),
        TdsDataType::SsVariant => 8000,
        // Everything else transfers at its wire width, and `type_info.length` is
        // already a byte count for every type including the national ones.
        _ => SqlLen::try_from(meta.type_info.length).unwrap_or(SqlLen::MAX),
    }
}

/// `SQL_DESC_PRECISION`: fractional-seconds precision for the temporal types,
/// binary precision for the approximate numerics, otherwise the number of
/// significant digits.
pub(super) fn precision(meta: &ColumnMetadata) -> SqlSmallInt {
    if matches!(
        meta.data_type,
        TdsDataType::DateN
            | TdsDataType::TimeN
            | TdsDataType::DateTime
            | TdsDataType::DateTim4
            | TdsDataType::DateTimeN
            | TdsDataType::DateTime2N
            | TdsDataType::DateTimeOffsetN
    ) {
        return decimal_digits(meta);
    }
    if let Some(binary) = binary_precision(meta) {
        return binary;
    }
    if let Some(p) = meta.get_precision() {
        return SqlSmallInt::from(p);
    }
    SqlSmallInt::try_from(column_size(meta)).unwrap_or(SqlSmallInt::MAX)
}

/// `SQL_DESC_SEARCHABLE`: which predicates the server accepts for the column.
fn searchable(meta: &ColumnMetadata) -> SqlLen {
    match meta.data_type {
        // The legacy LOB text types accept `LIKE` but not comparison.
        TdsDataType::Text | TdsDataType::NText => SQL_PRED_CHAR,
        // Not usable in a `WHERE` clause without a conversion or a method call.
        TdsDataType::Image | TdsDataType::Xml | TdsDataType::Udt | TdsDataType::Vector => {
            SQL_PRED_NONE
        }
        // Character and calendar types take every predicate including `LIKE`.
        TdsDataType::Char
        | TdsDataType::BigChar
        | TdsDataType::VarChar
        | TdsDataType::BigVarChar
        | TdsDataType::NChar
        | TdsDataType::NVarChar
        | TdsDataType::DateN
        | TdsDataType::TimeN
        | TdsDataType::DateTime
        | TdsDataType::DateTim4
        | TdsDataType::DateTimeN
        | TdsDataType::DateTime2N
        | TdsDataType::DateTimeOffsetN => SQL_PRED_SEARCHABLE,
        // Numerics, binary, bit, GUID and sql_variant compare but do not `LIKE`.
        _ => SQL_PRED_BASIC,
    }
}

fn num_prec_radix(meta: &ColumnMetadata) -> SqlLen {
    match meta.data_type {
        TdsDataType::Flt4 | TdsDataType::Flt8 | TdsDataType::FltN => 2,
        TdsDataType::Int1
        | TdsDataType::Int2
        | TdsDataType::Int4
        | TdsDataType::Int8
        | TdsDataType::IntN
        | TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => 10,
        // Non-numeric columns have no radix.
        _ => 0,
    }
}

/// ODBC 3.8 introduced the `SQL_C_SS_*` mappings for variant `time` and
/// `datetimeoffset`; earlier or unset versions use the binary fallback
/// (`IS351ORLESSAPP`, `sqlcdesc.cpp:6474`).
fn uses_3_80_variant_types(odbc_version: OdbcVersion) -> bool {
    odbc_version == OdbcVersion::Odbc3_80
}

/// The C type a `sql_variant` value reports for `SQL_CA_SS_VARIANT_TYPE`.
///
/// msodbcsql answers this from its per-row column info, so the value's base type
/// decides it rather than the column's declared type.
fn variant_c_type(base: TdsDataType, uses_3_80_types: bool) -> SqlSmallInt {
    match base {
        // `tinyint` is unsigned 0-255 on the server, so the unsigned C type is
        // the accurate answer: a caller that fetched 200 into a signed char
        // through SQL_C_TINYINT would read -56. Measured against retail
        // msodbcsql18 18.06.0001, which answers SQL_C_UTINYINT (-28), and
        // consistent with this driver's own `type_rules::resolve_default_c_type`,
        // which resolves SQL_TINYINT to SQL_C_UTINYINT for a column binding.
        TdsDataType::Int1 => SQL_C_UTINYINT,
        TdsDataType::Int2 => SQL_C_SSHORT,
        TdsDataType::Int4 => SQL_C_SLONG,
        TdsDataType::Int8 => SQL_C_SBIGINT,
        TdsDataType::Bit | TdsDataType::BitN => SQL_C_BIT,
        TdsDataType::Flt4 => SQL_C_FLOAT,
        TdsDataType::Flt8 | TdsDataType::FltN => SQL_C_DOUBLE,
        // Measured against retail msodbcsql18 18.06.0001: decimal, numeric and
        // money all answer SQL_C_NUMERIC. Money belongs here because it is
        // reported as SQL_DECIMAL in the column form too.
        //
        // Known cost, stated plainly: this answer is not yet serviceable. A
        // caller that follows the ODBC data-type-mapping workflow and binds the
        // returned C type gets HYC00 from SQLGetData / SQLBindCol until AB#47816
        // lands, where msodbcsql succeeds. That mismatch is new here - the
        // previous answer, SQL_C_CHAR, was always deliverable.
        //
        // Shipped in this order deliberately. The alternative was to keep
        // answering SQL_C_CHAR, which is not a safe holding position: it is the
        // wrong answer, and it fails silently. mssql-python routes on this value
        // alone and handed back `str` where the value is a decimal (AB#47702),
        // with nothing to indicate the type was wrong. A defined
        // "optional feature not implemented" is a better failure than data of
        // the wrong shape, and it only reaches a caller that asks for the struct
        // this driver has never offered on any path.
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => SQL_C_NUMERIC,
        // GetIRDField reports the legacy C codes for variant date and datetime
        // at every ODBC version (`sqlcdesc.cpp:6462-6469`).
        TdsDataType::DateN => SQL_C_DATE,
        TdsDataType::TimeN if uses_3_80_types => SQL_C_SS_TIME2,
        TdsDataType::TimeN => SQL_C_BINARY,
        TdsDataType::DateTime | TdsDataType::DateTim4 | TdsDataType::DateTimeN => SQL_C_TIMESTAMP,
        TdsDataType::DateTime2N => SQL_C_TIMESTAMP,
        TdsDataType::DateTimeOffsetN if uses_3_80_types => SQL_C_SS_TIMESTAMPOFFSET,
        TdsDataType::DateTimeOffsetN => SQL_C_BINARY,
        TdsDataType::Char
        | TdsDataType::BigChar
        | TdsDataType::VarChar
        | TdsDataType::BigVarChar => SQL_C_CHAR,
        TdsDataType::NChar | TdsDataType::NVarChar => SQL_C_WCHAR,
        TdsDataType::Binary
        | TdsDataType::BigBinary
        | TdsDataType::VarBinary
        | TdsDataType::BigVarBinary => SQL_C_BINARY,
        TdsDataType::Guid => SQL_C_GUID,
        // Enumerated rather than left to a `_` arm: every type below is one a
        // variant provably cannot carry, and spelling them out is what makes
        // adding a `TdsDataType` a compile error here instead of a silent
        // `SQL_C_CHAR`. Character stays the answer for all of them, so this is
        // about the next person's mistake, not about today's behavior.
        //
        // `IntN` is the non-obvious one: a variant carries an integer as
        // `INT1/2/4/8TYPE` and never as `INTNTYPE` (MS-TDS 2.2.5.5.2), so the
        // nullable spelling never reaches the wire inside a variant even though
        // the other `*N` types do.
        //
        // The rest SQL Server rejects when the value is assigned to a
        // `sql_variant`: the LOB types, the structured and CLR types, a nested
        // variant, and `Void`/`None`, which are placeholders rather than column
        // types at all.
        TdsDataType::IntN
        | TdsDataType::Image
        | TdsDataType::Text
        | TdsDataType::NText
        | TdsDataType::Xml
        | TdsDataType::Json
        | TdsDataType::Vector
        | TdsDataType::Udt
        | TdsDataType::SqlTable
        | TdsDataType::SsVariant
        | TdsDataType::Void
        | TdsDataType::None => SQL_C_CHAR,
    }
}

/// `SQL_DESC_UNSIGNED` is `SQL_FALSE` only for the signed numeric types. Every
/// nonnumeric column — character, binary, temporal, GUID, bit, sql_variant —
/// reports `SQL_TRUE`, as does `tinyint`, the one unsigned integer SQL Server
/// exposes.
///
/// The test is on the ODBC type rather than the TDS type, which is what makes
/// `money` come out signed: it is reported as `SQL_DECIMAL`.
fn is_unsigned(meta: &ColumnMetadata) -> bool {
    !matches!(
        odbc_sql_type(meta),
        SQL_SMALLINT
            | SQL_INTEGER
            | SQL_BIGINT
            | SQL_REAL
            | SQL_FLOAT
            | SQL_DOUBLE
            | SQL_DECIMAL
            | SQL_NUMERIC
    )
}

fn type_name(meta: &ColumnMetadata) -> &'static str {
    match meta.data_type {
        TdsDataType::Int1 => "tinyint",
        TdsDataType::Int2 => "smallint",
        TdsDataType::Int4 => "int",
        TdsDataType::Int8 => "bigint",
        TdsDataType::IntN => match meta.type_info.length {
            1 => "tinyint",
            2 => "smallint",
            4 => "int",
            8 => "bigint",
            _ => "int",
        },
        TdsDataType::Bit | TdsDataType::BitN => "bit",
        TdsDataType::Flt4 => "real",
        TdsDataType::Flt8 => "float",
        TdsDataType::FltN => {
            if meta.type_info.length == 4 {
                "real"
            } else {
                "float"
            }
        }
        TdsDataType::Decimal | TdsDataType::DecimalN => "decimal",
        TdsDataType::Numeric | TdsDataType::NumericN => "numeric",
        TdsDataType::Money | TdsDataType::MoneyN => "money",
        TdsDataType::Money4 => "smallmoney",
        TdsDataType::DateN => "date",
        TdsDataType::TimeN => "time",
        TdsDataType::DateTime | TdsDataType::DateTimeN => "datetime",
        TdsDataType::DateTim4 => "smalldatetime",
        TdsDataType::DateTime2N => "datetime2",
        TdsDataType::DateTimeOffsetN => "datetimeoffset",
        TdsDataType::Char | TdsDataType::BigChar => "char",
        TdsDataType::VarChar | TdsDataType::BigVarChar => "varchar",
        TdsDataType::Text => "text",
        TdsDataType::NChar => "nchar",
        TdsDataType::NVarChar => "nvarchar",
        TdsDataType::NText => "ntext",
        TdsDataType::Binary | TdsDataType::BigBinary => "binary",
        TdsDataType::VarBinary | TdsDataType::BigVarBinary => "varbinary",
        TdsDataType::Image => "image",
        TdsDataType::Guid => "uniqueidentifier",
        TdsDataType::Xml => "xml",
        TdsDataType::Json => "json",
        TdsDataType::Vector => "vector",
        TdsDataType::SsVariant => "sql_variant",
        TdsDataType::Udt => "udt",
        _ => "unknown",
    }
}

// Most per-type mapping tables are covered end-to-end by
// `tests/e2e/tests/col_attribute_test.cpp` against a live SQL Server.
#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{
        SQL_NULLABLE, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_TYPE_DATE, SQL_TYPE_TIMESTAMP,
    };
    use crate::api::sqlstate::ERR_INVALID_DESCRIPTOR_FIELD;
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sqldatatypes::TypeInfo;
    use mssql_tds::test_client_support::{int_columns, udt_column};

    /// A statement positioned on a result set of `n` nullable `int` columns.
    fn stmt_with_int_columns(h: &TestHandles, n: usize) {
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut s = stmt_handle.inner.lock().unwrap();
        s.set_state(STMT_STATE_EXEC_CONTEXT);
        s.column_metadata = int_columns(n);
    }

    /// Reads a numeric attribute, asserting the call succeeded.
    fn numeric(h: &TestHandles, col: SqlUSmallInt, field: SqlUSmallInt) -> SqlLen {
        let mut out: SqlLen = -1;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                col,
                field,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "field {field}");
        out
    }

    /// Reads a string attribute, asserting the call succeeded.
    fn text(h: &TestHandles, col: SqlUSmallInt, field: SqlUSmallInt) -> String {
        let mut buf = [0u16; 64];
        let mut written: SqlSmallInt = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                col,
                field,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * 2) as SqlSmallInt,
                &mut written,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "field {field}");
        String::from_utf16_lossy(&buf[..(written as usize) / 2])
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                ptr::null_mut(),
                1,
                SQL_DESC_CONCISE_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn fresh_stmt_returns_sequence_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_CONCISE_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_FUNCTION_SEQUENCE.state
        );
    }

    #[test]
    fn column_out_of_range_is_invalid_descriptor_index() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        for col in [0, 3] {
            let mut out: SqlLen = 0;
            let rc = unsafe {
                sql_col_attribute_w(
                    h.stmt,
                    col,
                    SQL_DESC_CONCISE_TYPE,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut out,
                )
            };
            assert_eq!(rc, SQL_ERROR, "column {col}");
            let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = sh.inner.lock().unwrap();
            assert_eq!(
                s.diag_records.last().unwrap().sql_state,
                ERR_INVALID_DESCRIPTOR_INDEX.state
            );
        }
    }

    /// An identifier this driver does not report is HY091, not a silent zero.
    #[test]
    fn unknown_field_identifier_is_invalid_descriptor_field() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                9999,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_INVALID_DESCRIPTOR_FIELD.state
        );
    }

    /// SQL_DESC_COUNT describes the result set, so it answers even for a column
    /// number that would otherwise be out of range.
    #[test]
    fn desc_count_ignores_column_number() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 3);
        assert_eq!(numeric(&h, 0, SQL_DESC_COUNT), 3);
        assert_eq!(numeric(&h, 99, SQL_DESC_COUNT), 3);
    }

    #[test]
    fn int_column_numeric_attributes() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        assert_eq!(
            numeric(&h, 1, SQL_DESC_CONCISE_TYPE),
            SqlLen::from(SQL_INTEGER)
        );
        assert_eq!(numeric(&h, 1, SQL_DESC_TYPE), SqlLen::from(SQL_INTEGER));
        assert_eq!(
            numeric(&h, 1, SQL_DESC_NULLABLE),
            SqlLen::from(SQL_NULLABLE)
        );
        // `int` is signed, and base 10.
        assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 0);
        assert_eq!(numeric(&h, 1, SQL_DESC_NUM_PREC_RADIX), 10);
        assert_eq!(numeric(&h, 1, SQL_DESC_UNNAMED), SQL_NAMED);
    }

    /// The wide entry point reports the name length in bytes, and a short buffer
    /// truncates with 01004 rather than failing.
    #[test]
    fn name_is_written_as_utf16_with_byte_length() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let mut buf = [0u16; 16];
        let mut len: SqlSmallInt = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_NAME,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * 2) as SqlSmallInt,
                &mut len,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        // "c1" is two characters, so four bytes.
        assert_eq!(len, 4);
        let name = String::from_utf16_lossy(&buf[..2]);
        assert_eq!(name, "c1");

        let mut small = [0u16; 2];
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_NAME,
                small.as_mut_ptr() as SqlPointer,
                (small.len() * 2) as SqlSmallInt,
                &mut len,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            WARN_STRING_TRUNCATION.state
        );
    }

    /// The variant attribute is rejected outright on a column that is not a
    /// `sql_variant`, rather than reporting a type the caller would then trust.
    #[test]
    fn variant_type_on_non_variant_column_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_CA_SS_VARIANT_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_NOT_VARIANT_COLUMN.state
        );
    }

    /// Retypes column `col` (1-based) in place. `int_columns` is the only
    /// metadata constructor available here, and the fields are public, so this
    /// is how the per-type mapping tables get exercised without a live server.
    fn retype_column(h: &TestHandles, col: usize, data_type: TdsDataType, length: usize) {
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut s = stmt_handle.inner.lock().unwrap();
        let meta = &mut s.column_metadata[col - 1];
        meta.data_type = data_type;
        meta.type_info.tds_type = data_type;
        meta.type_info.length = length;
    }

    /// Every numeric attribute this driver reports, on one `int` column.
    #[test]
    fn every_numeric_attribute_answers_for_an_int_column() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        assert_eq!(numeric(&h, 1, SQL_DESC_LENGTH), 10);
        // Display size is the column size plus a character for the sign.
        assert_eq!(numeric(&h, 1, SQL_DESC_DISPLAY_SIZE), 11);
        assert_eq!(numeric(&h, 1, SQL_DESC_OCTET_LENGTH), 4);
        assert_eq!(numeric(&h, 1, SQL_DESC_PRECISION), 10);
        assert_eq!(numeric(&h, 1, SQL_DESC_SCALE), 0);
        assert_eq!(numeric(&h, 1, SQL_DESC_CASE_SENSITIVE), 0);
        assert_eq!(numeric(&h, 1, SQL_DESC_FIXED_PREC_SCALE), 0);
        assert_eq!(
            numeric(&h, 1, SQL_DESC_UPDATABLE),
            SQL_ATTR_READWRITE_UNKNOWN
        );
        assert_eq!(numeric(&h, 1, SQL_DESC_AUTO_UNIQUE_VALUE), 0);
        // An int compares but does not take LIKE.
        assert_eq!(numeric(&h, 1, SQL_DESC_SEARCHABLE), SQL_PRED_BASIC);
        // A signed numeric is the one shape that is not "unsigned".
        assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 0);
    }

    /// A column with no name reports `SQL_UNNAMED`; `int_columns` names them.
    #[test]
    fn unnamed_column_is_reported() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata[0].column_name.clear();
        }
        assert_eq!(numeric(&h, 1, SQL_DESC_UNNAMED), SQL_UNNAMED);
    }

    /// A non-nullable column reports `SQL_NO_NULLS`. `int_columns` sets the
    /// nullable flag, so clear it.
    #[test]
    fn not_nullable_column_is_reported() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata[0].flags &= !0x01;
        }
        assert_eq!(
            numeric(&h, 1, SQL_DESC_NULLABLE),
            SqlLen::from(SQL_NO_NULLS)
        );
    }

    #[test]
    fn type_name_and_radix_track_the_column_type() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        // (type, wire length, expected type name, expected radix)
        let cases: &[(TdsDataType, usize, &str, SqlLen)] = &[
            (TdsDataType::Int1, 1, "tinyint", 10),
            (TdsDataType::Int2, 2, "smallint", 10),
            (TdsDataType::Int8, 8, "bigint", 10),
            (TdsDataType::Flt4, 4, "real", 2),
            (TdsDataType::Flt8, 8, "float", 2),
            (TdsDataType::MoneyN, 8, "money", 10),
            (TdsDataType::Guid, 16, "uniqueidentifier", 0),
            (TdsDataType::BigVarChar, 10, "varchar", 0),
            (TdsDataType::NVarChar, 20, "nvarchar", 0),
            (TdsDataType::SsVariant, 8, "sql_variant", 0),
        ];
        for (ty, len, name, radix) in cases {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(numeric(&h, 1, SQL_DESC_NUM_PREC_RADIX), *radix, "{ty:?}");
            assert_eq!(text(&h, 1, SQL_DESC_TYPE_NAME), *name, "{ty:?}");
        }
    }

    /// `SQL_DESC_UNSIGNED` is false only for the signed numerics. Everything
    /// nonnumeric is "unsigned" by the ODBC definition, which is the opposite of
    /// the intuitive reading.
    #[test]
    fn unsigned_is_false_only_for_signed_numerics() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let signed: &[(TdsDataType, usize)] = &[
            (TdsDataType::Int2, 2),
            (TdsDataType::Int4, 4),
            (TdsDataType::Int8, 8),
            (TdsDataType::IntN, 4),
            (TdsDataType::Flt4, 4),
            (TdsDataType::Flt8, 8),
            (TdsDataType::DecimalN, 9),
            (TdsDataType::NumericN, 9),
            // money is reported as SQL_DECIMAL, which is a signed numeric.
            (TdsDataType::MoneyN, 8),
        ];
        for (ty, len) in signed {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 0, "{ty:?}");
        }

        let unsigned: &[(TdsDataType, usize)] = &[
            (TdsDataType::Int1, 1),
            (TdsDataType::IntN, 1),
            (TdsDataType::Bit, 1),
            (TdsDataType::BigVarChar, 10),
            (TdsDataType::NVarChar, 20),
            (TdsDataType::BigVarBinary, 8),
            (TdsDataType::DateN, 3),
            (TdsDataType::DateTime2N, 8),
            (TdsDataType::Guid, 16),
            (TdsDataType::SsVariant, 8),
        ];
        for (ty, len) in unsigned {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 1, "{ty:?}");
        }
    }

    /// Display size is the rendered width, which differs from the column size
    /// for every type that needs a sign, a separator, or hex expansion.
    #[test]
    fn display_size_is_the_rendered_width() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let cases: &[(TdsDataType, usize, SqlLen)] = &[
            (TdsDataType::Bit, 1, 1),
            // tinyint has no sign, so it stays at three digits.
            (TdsDataType::Int1, 1, 3),
            (TdsDataType::Int2, 2, 6),
            (TdsDataType::Int4, 4, 11),
            (TdsDataType::Int8, 8, 20),
            (TdsDataType::Flt4, 4, 14),
            (TdsDataType::Flt8, 8, 24),
            (TdsDataType::MoneyN, 8, 21),
            (TdsDataType::MoneyN, 4, 12),
            // 32 hex digits and 4 dashes.
            (TdsDataType::Guid, 16, 36),
            // Two hex characters per byte.
            (TdsDataType::BigVarBinary, 8, 16),
            // Characters, not bytes.
            (TdsDataType::NVarChar, 20, 10),
            (TdsDataType::BigVarChar, 10, 10),
            (TdsDataType::DateN, 3, 10),
            (TdsDataType::SsVariant, 8, 8000),
        ];
        for (ty, len, expected) in cases {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(numeric(&h, 1, SQL_DESC_DISPLAY_SIZE), *expected, "{ty:?}");
        }
    }

    /// Octet length is the size of the ODBC transfer representation, so the
    /// temporal types report their C struct size rather than the TDS payload
    /// width, and the exact numerics report their rendered width.
    #[test]
    fn octet_length_is_the_transfer_size() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let cases: &[(TdsDataType, usize, SqlLen)] = &[
            // SQL_DATE_STRUCT, against a 3-byte wire payload.
            (TdsDataType::DateN, 3, 6),
            // SQL_SS_TIME2_STRUCT, against a 5-byte wire payload.
            (TdsDataType::TimeN, 5, 12),
            // SQL_TIMESTAMP_STRUCT.
            (TdsDataType::DateTime, 8, 16),
            (TdsDataType::DateTim4, 4, 16),
            (TdsDataType::DateTime2N, 8, 16),
            // SQL_SS_TIMESTAMPOFFSET_STRUCT, against a 10-byte wire payload.
            (TdsDataType::DateTimeOffsetN, 10, 20),
            // The exact numerics transfer as characters.
            (TdsDataType::MoneyN, 8, 21),
            (TdsDataType::MoneyN, 4, 12),
            // The fixed-width and character types transfer at their wire width.
            (TdsDataType::Int4, 4, 4),
            (TdsDataType::Guid, 16, 16),
            (TdsDataType::BigVarChar, 10, 10),
            // Bytes, not characters.
            (TdsDataType::NVarChar, 20, 20),
        ];
        for (ty, len, expected) in cases {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(numeric(&h, 1, SQL_DESC_OCTET_LENGTH), *expected, "{ty:?}");
        }
    }

    /// The timestamp family reports the verbose `SQL_DATETIME` for
    /// `SQL_DESC_TYPE` and the concise type separately; every other type reports
    /// the same value for both.
    #[test]
    fn verbose_type_differs_from_concise_only_for_timestamps() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        for (ty, len) in [
            (TdsDataType::DateTime, 8),
            (TdsDataType::DateTim4, 4),
            (TdsDataType::DateTime2N, 8),
        ] {
            retype_column(&h, 1, ty, len);
            assert_eq!(numeric(&h, 1, SQL_DESC_TYPE), SqlLen::from(SQL_DATETIME));
            assert_eq!(
                numeric(&h, 1, SQL_DESC_CONCISE_TYPE),
                SqlLen::from(SQL_TYPE_TIMESTAMP)
            );
            assert_eq!(
                numeric(&h, 1, SQL_DESC_DATETIME_INTERVAL_CODE),
                SQL_CODE_TIMESTAMP
            );
        }

        // date, time and datetimeoffset use types outside the ODBC datetime
        // range, so the verbose and concise fields agree and no subtype applies.
        for (ty, len, expected) in [
            (TdsDataType::DateN, 3, SQL_TYPE_DATE),
            (TdsDataType::TimeN, 5, SQL_SS_TIME2),
            (TdsDataType::DateTimeOffsetN, 10, SQL_SS_TIMESTAMPOFFSET),
            (TdsDataType::Int4, 4, SQL_INTEGER),
        ] {
            retype_column(&h, 1, ty, len);
            assert_eq!(numeric(&h, 1, SQL_DESC_TYPE), SqlLen::from(expected));
            assert_eq!(
                numeric(&h, 1, SQL_DESC_CONCISE_TYPE),
                SqlLen::from(expected)
            );
            assert_eq!(numeric(&h, 1, SQL_DESC_DATETIME_INTERVAL_CODE), 0);
        }
    }

    /// Precision is fractional-seconds for the temporal types and binary
    /// precision for the approximate numerics, not the display width.
    #[test]
    fn precision_is_type_specific() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let cases: &[(TdsDataType, usize, SqlSmallInt)] = &[
            (TdsDataType::Flt4, 4, 24),
            (TdsDataType::Flt8, 8, 53),
            // datetime is fixed at three fractional digits, smalldatetime at none.
            (TdsDataType::DateTime, 8, 3),
            (TdsDataType::DateTim4, 4, 0),
            (TdsDataType::DateN, 3, 0),
            (TdsDataType::Int4, 4, 10),
        ];
        for (ty, len, expected) in cases {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(
                numeric(&h, 1, SQL_DESC_PRECISION),
                SqlLen::from(*expected),
                "{ty:?}"
            );
        }
    }

    /// Searchability is derived from the type: the LOB text types take only
    /// `LIKE`, xml and image take neither, and the rest compare.
    #[test]
    fn searchable_is_derived_from_the_type() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let cases: &[(TdsDataType, usize, SqlLen)] = &[
            (TdsDataType::Text, 16, SQL_PRED_CHAR),
            (TdsDataType::NText, 16, SQL_PRED_CHAR),
            (TdsDataType::Image, 16, SQL_PRED_NONE),
            (TdsDataType::Xml, 0, SQL_PRED_NONE),
            (TdsDataType::Udt, 0, SQL_PRED_NONE),
            (TdsDataType::BigVarChar, 10, SQL_PRED_SEARCHABLE),
            (TdsDataType::NVarChar, 20, SQL_PRED_SEARCHABLE),
            (TdsDataType::DateN, 3, SQL_PRED_SEARCHABLE),
            (TdsDataType::DateTime2N, 8, SQL_PRED_SEARCHABLE),
            (TdsDataType::Int4, 4, SQL_PRED_BASIC),
            (TdsDataType::Guid, 16, SQL_PRED_BASIC),
            (TdsDataType::BigVarBinary, 8, SQL_PRED_BASIC),
            (TdsDataType::SsVariant, 8, SQL_PRED_BASIC),
        ];
        for (ty, len, expected) in cases {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(numeric(&h, 1, SQL_DESC_SEARCHABLE), *expected, "{ty:?}");
        }
    }

    /// An IDENTITY column is an auto-unique value.
    #[test]
    fn identity_column_reports_auto_unique_value() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata[0].flags |= 0x10;
        }
        assert_eq!(numeric(&h, 1, SQL_DESC_AUTO_UNIQUE_VALUE), 1);
    }

    /// COLMETADATA carries no originating column, so ODBC's "provenance
    /// unknown" answer is an empty string — not the alias the label reports.
    #[test]
    fn base_column_name_is_empty_while_the_label_is_the_alias() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata[0].column_name = "alias".to_string();
        }
        assert_eq!(text(&h, 1, SQL_DESC_NAME), "alias");
        assert_eq!(text(&h, 1, SQL_DESC_LABEL), "alias");
        assert_eq!(text(&h, 1, SQL_DESC_BASE_COLUMN_NAME), "");
    }

    /// The C type reported for each base type a `sql_variant` can carry.
    /// Exercised directly because a variant's base type is a property of the
    /// value, which unit tests cannot produce.
    #[test]
    fn variant_c_type_covers_the_base_types() {
        let cases: &[(TdsDataType, SqlSmallInt, SqlSmallInt)] = &[
            (TdsDataType::Int1, SQL_C_UTINYINT, SQL_C_UTINYINT),
            (TdsDataType::Int2, SQL_C_SSHORT, SQL_C_SSHORT),
            (TdsDataType::Int4, SQL_C_SLONG, SQL_C_SLONG),
            (TdsDataType::Int8, SQL_C_SBIGINT, SQL_C_SBIGINT),
            (TdsDataType::Bit, SQL_C_BIT, SQL_C_BIT),
            (TdsDataType::Flt4, SQL_C_FLOAT, SQL_C_FLOAT),
            (TdsDataType::Flt8, SQL_C_DOUBLE, SQL_C_DOUBLE),
            (TdsDataType::Numeric, SQL_C_NUMERIC, SQL_C_NUMERIC),
            (TdsDataType::DecimalN, SQL_C_NUMERIC, SQL_C_NUMERIC),
            (TdsDataType::MoneyN, SQL_C_NUMERIC, SQL_C_NUMERIC),
            (TdsDataType::Money4, SQL_C_NUMERIC, SQL_C_NUMERIC),
            (TdsDataType::DateN, SQL_C_DATE, SQL_C_DATE),
            (TdsDataType::TimeN, SQL_C_BINARY, SQL_C_SS_TIME2),
            (TdsDataType::DateTimeN, SQL_C_TIMESTAMP, SQL_C_TIMESTAMP),
            (TdsDataType::DateTime2N, SQL_C_TIMESTAMP, SQL_C_TIMESTAMP),
            (
                TdsDataType::DateTimeOffsetN,
                SQL_C_BINARY,
                SQL_C_SS_TIMESTAMPOFFSET,
            ),
            (TdsDataType::BigVarChar, SQL_C_CHAR, SQL_C_CHAR),
            (TdsDataType::NVarChar, SQL_C_WCHAR, SQL_C_WCHAR),
            (TdsDataType::BigVarBinary, SQL_C_BINARY, SQL_C_BINARY),
            (TdsDataType::Guid, SQL_C_GUID, SQL_C_GUID),
            // A variant cannot carry these, so character is the fallback.
            (TdsDataType::Xml, SQL_C_CHAR, SQL_C_CHAR),
        ];
        for (base, before_3_80, in_3_80) in cases {
            assert_eq!(variant_c_type(*base, false), *before_3_80, "{base:?}");
            assert_eq!(variant_c_type(*base, true), *in_3_80, "{base:?}");
        }
    }

    #[test]
    fn only_odbc_3_80_uses_extended_variant_types() {
        assert!(!uses_3_80_variant_types(OdbcVersion::Unset));
        assert!(!uses_3_80_variant_types(OdbcVersion::Odbc2));
        assert!(!uses_3_80_variant_types(OdbcVersion::Odbc3));
        assert!(uses_3_80_variant_types(OdbcVersion::Odbc3_80));
    }

    /// The success path: a variant column whose value has been probed reports
    /// that value's underlying C type.
    #[test]
    fn variant_type_is_reported_after_the_value_is_probed() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        retype_column(&h, 1, TdsDataType::SsVariant, 8);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.last_variant_base = Some((1, TdsDataType::NVarChar));
        }
        assert_eq!(
            numeric(&h, 1, SQL_CA_SS_VARIANT_TYPE),
            SqlLen::from(SQL_C_WCHAR)
        );
    }

    #[test]
    fn temporal_variant_type_honors_the_declared_odbc_version() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        retype_column(&h, 1, TdsDataType::SsVariant, 8);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt_handle.inner.lock().unwrap().last_variant_base = Some((1, TdsDataType::TimeN));
        }

        assert_eq!(
            numeric(&h, 1, SQL_CA_SS_VARIANT_TYPE),
            SqlLen::from(SQL_C_SS_TIME2)
        );

        {
            // Directly mutating the ENV after DBC allocation is test-only; a
            // Driver Manager rejects this state transition.
            let env_handle = unsafe { handle_from_raw::<crate::handles::EnvHandle>(h.env) };
            env_handle.inner.lock().unwrap().odbc_version = OdbcVersion::Odbc3;
        }
        assert_eq!(
            numeric(&h, 1, SQL_CA_SS_VARIANT_TYPE),
            SqlLen::from(SQL_C_BINARY)
        );
    }

    /// A base type captured for one column must not answer for another.
    #[test]
    fn variant_type_does_not_leak_across_columns() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        retype_column(&h, 1, TdsDataType::SsVariant, 8);
        retype_column(&h, 2, TdsDataType::SsVariant, 8);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.last_variant_base = Some((1, TdsDataType::Int4));
        }
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                2,
                SQL_CA_SS_VARIANT_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_FUNCTION_SEQUENCE.state
        );
    }

    /// Every arm of the type-name table. Driven directly because a name is a
    /// pure function of the metadata and needs no live result set.
    #[test]
    fn type_name_covers_every_supported_type() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let cases: &[(TdsDataType, usize, &str)] = &[
            (TdsDataType::Int1, 1, "tinyint"),
            (TdsDataType::Int2, 2, "smallint"),
            (TdsDataType::Int4, 4, "int"),
            (TdsDataType::Int8, 8, "bigint"),
            (TdsDataType::IntN, 1, "tinyint"),
            (TdsDataType::IntN, 2, "smallint"),
            (TdsDataType::IntN, 4, "int"),
            (TdsDataType::IntN, 8, "bigint"),
            // A length the server never sends still has to name something.
            (TdsDataType::IntN, 3, "int"),
            (TdsDataType::Bit, 1, "bit"),
            (TdsDataType::BitN, 1, "bit"),
            (TdsDataType::Flt4, 4, "real"),
            (TdsDataType::Flt8, 8, "float"),
            (TdsDataType::FltN, 4, "real"),
            (TdsDataType::FltN, 8, "float"),
            (TdsDataType::Decimal, 9, "decimal"),
            (TdsDataType::DecimalN, 9, "decimal"),
            (TdsDataType::Numeric, 9, "numeric"),
            (TdsDataType::NumericN, 9, "numeric"),
            (TdsDataType::Money, 8, "money"),
            (TdsDataType::MoneyN, 8, "money"),
            (TdsDataType::Money4, 4, "smallmoney"),
            (TdsDataType::DateN, 3, "date"),
            (TdsDataType::TimeN, 5, "time"),
            (TdsDataType::DateTime, 8, "datetime"),
            (TdsDataType::DateTimeN, 8, "datetime"),
            (TdsDataType::DateTim4, 4, "smalldatetime"),
            (TdsDataType::DateTime2N, 8, "datetime2"),
            (TdsDataType::DateTimeOffsetN, 10, "datetimeoffset"),
            (TdsDataType::Char, 10, "char"),
            (TdsDataType::BigChar, 10, "char"),
            (TdsDataType::VarChar, 10, "varchar"),
            (TdsDataType::BigVarChar, 10, "varchar"),
            (TdsDataType::Text, 16, "text"),
            (TdsDataType::NChar, 20, "nchar"),
            (TdsDataType::NVarChar, 20, "nvarchar"),
            (TdsDataType::NText, 16, "ntext"),
            (TdsDataType::Binary, 8, "binary"),
            (TdsDataType::BigBinary, 8, "binary"),
            (TdsDataType::VarBinary, 8, "varbinary"),
            (TdsDataType::BigVarBinary, 8, "varbinary"),
            (TdsDataType::Image, 16, "image"),
            (TdsDataType::Guid, 16, "uniqueidentifier"),
            (TdsDataType::Xml, 0, "xml"),
            (TdsDataType::Json, 0, "json"),
            (TdsDataType::Vector, 0, "vector"),
            (TdsDataType::SsVariant, 8, "sql_variant"),
            (TdsDataType::Udt, 0, "udt"),
            (TdsDataType::Void, 0, "unknown"),
        ];
        for (ty, len, name) in cases {
            retype_column(&h, 1, *ty, *len);
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = stmt_handle.inner.lock().unwrap();
            assert_eq!(type_name(&s.column_metadata[0]), *name, "{ty:?} len {len}");
        }
    }

    /// A `varchar(max)` streams as PLP, which has no fixed octet length, so
    /// the driver reports zero rather than the sentinel wire length.
    #[test]
    fn plp_column_reports_zero_octet_length() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            let meta = &mut s.column_metadata[0];
            meta.data_type = TdsDataType::BigVarChar;
            meta.type_info = TypeInfo::partial_len(TdsDataType::BigVarChar, 0xFFFF, None)
                .expect("varchar(max) is a PLP type");
        }
        assert_eq!(numeric(&h, 1, SQL_DESC_OCTET_LENGTH), 0);
    }

    #[test]
    fn udt_size_attributes_match_bounded_and_unbounded_metadata() {
        for (max_byte_size, expected_size) in [(u16::MAX, 0), (892, 892)] {
            let meta = udt_column(max_byte_size);

            assert_eq!(desc_length(&meta), expected_size);
            assert_eq!(SqlLen::from(precision(&meta)), expected_size);
            assert_eq!(octet_length(&meta), expected_size);
            assert_eq!(display_size(&meta), 0);
        }
    }

    /// A `decimal` carries its own precision on the wire, which takes
    /// precedence over the display size fallback.
    #[test]
    fn decimal_reports_its_declared_precision() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            let meta = &mut s.column_metadata[0];
            meta.data_type = TdsDataType::DecimalN;
            meta.type_info = TypeInfo::var_len_precision_scale(TdsDataType::DecimalN, 9, 18, 4)
                .expect("decimal carries precision and scale");
        }
        assert_eq!(numeric(&h, 1, SQL_DESC_PRECISION), 18);
        assert_eq!(numeric(&h, 1, SQL_DESC_SCALE), 4);
    }

    /// mssql-python passes a null string buffer and reads only the numeric
    /// attribute, so a null `character_attribute_ptr` must not fault.
    #[test]
    fn null_output_pointers_are_tolerated() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_NAME,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
    }
}
