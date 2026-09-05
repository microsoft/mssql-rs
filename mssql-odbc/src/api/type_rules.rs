// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Static ODBC rules about type identifiers: which `SQL_C_*` / `SQL_*` values
//! name real ODBC types, and which C type a SQL type defaults to.
//!
//! Distinct from [`crate::api::odbc_types`], which defines the identifiers
//! themselves. Direction-neutral and deliberately outside `params`: msodbcsql
//! shares `IsValidCType` between its APD and ARD paths (`SetADRec` serves both
//! `SQLBindParameter` and `SQLBindCol`) and keeps these predicates in the shared
//! `sqlcprot.h`.
//!
//! This driver targets ODBC 3.x only, which scopes out support for ODBC 2.x
//! *applications* — not the 2.x-era *identifiers*. `SQL_C_DATE` / `SQL_C_TIME` /
//! `SQL_C_TIMESTAMP` are deprecated but still defined in the ODBC 3.x headers, so
//! a 3.x application may legally pass them and the Driver Manager remaps nothing.
//! [`canonical_c_type`] folds them onto the `SQL_C_TYPE_*` forms, and everything
//! after it — validation, conversion, storage — sees only the canonical form.
//! msodbcsql is the mirror image: it folds 3.x down to 2.x, and its
//! `IsValidCType` likewise accepts only its own canonical (2.x) spelling.
//!
//! Validity here means only that an identifier names a real ODBC type. Whether a
//! particular pairing can be converted is decided per direction — for input
//! parameters by [`crate::params::conversion_matrix`], and for fetch inside the
//! converters themselves. SQL types are the exception: a real ODBC identifier
//! SQL Server has no counterpart for is rejected outright by
//! [`classify_parameter_sql_type`], never reaching a conversion check.

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DATE,
    SQL_C_DEFAULT, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID, SQL_C_INTERVAL_MINUTE_TO_SECOND,
    SQL_C_INTERVAL_YEAR, SQL_C_LONG, SQL_C_NUMERIC, SQL_C_SBIGINT, SQL_C_SHORT, SQL_C_SLONG,
    SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SS_VECTOR, SQL_C_SSHORT, SQL_C_STINYINT,
    SQL_C_TIMESTAMP, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP,
    SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_C_WCHAR, SQL_CHAR, SQL_DECIMAL,
    SQL_DOUBLE, SQL_FLOAT, SQL_GUID, SQL_INTEGER, SQL_INTERVAL_MINUTE_TO_SECOND, SQL_INTERVAL_YEAR,
    SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NUMERIC, SQL_REAL, SQL_SMALLINT, SQL_SS_TABLE,
    SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SS_UDT, SQL_SS_VARIANT, SQL_SS_VECTOR, SQL_SS_XML,
    SQL_TIMESTAMP, SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY,
    SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlDateStruct, SqlGuid,
    SqlNumericStruct, SqlSmallInt, SqlSsTime2Struct, SqlSsTimestampoffsetStruct, SqlTimeStruct,
    SqlTimestampStruct,
};
use crate::handles::OdbcVersion;

/// Offset between the ODBC 2.x (`9`..`11`) and 3.x (`91`..`93`) date/time ids.
const ODBC2_DATETIME_OFFSET: SqlSmallInt = SQL_C_TYPE_DATE - SQL_C_DATE;

// Maximum declarable precision per SQL type, in characters. Names and values
// both match `Sql/Ntdbms/sqlncli/tds/tds.h`, so a cross-check against msodbcsql
// is a direct comparison. Note that its `SQL_PREC_CHARBINARY` is a *different*
// constant, 255 - the TDS 4.2 `SQLCHARACTER` limit that `SQLBIGCHAR` superseded
// and that this driver never declares.
pub(crate) const SQL_PREC_BIGCHARBINARY: usize = 8000;
pub(crate) const SQL_PREC_NCHAR: usize = 4000;
pub(crate) const SQL_PREC_TEXTIMAGE: usize = 2_147_483_647;
pub(crate) const SQL_PREC_NTEXT: usize = 1_073_741_823;
pub(crate) const SQL_PREC_NUMERIC: usize = 38;
/// `ColumnSize` 0, which only the `max`-capable types accept.
pub(crate) const SQL_PREC_UNLIMITED: usize = 0;

/// Folds the deprecated 2.x date/time C spellings onto their `SQL_C_TYPE_*`
/// equivalents so everything downstream sees one form per type.
///
/// msodbcsql canonicalizes the same pair in `SQLBindParameter`
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp`), just toward the 2.x values, which
/// are its internal representation.
pub(crate) fn canonical_c_type(c_type: SqlSmallInt) -> SqlSmallInt {
    if (SQL_C_DATE..=SQL_C_TIMESTAMP).contains(&c_type) {
        c_type + ODBC2_DATETIME_OFFSET
    } else {
        c_type
    }
}

/// Folds the unambiguous ODBC 2.x `SQL_TIMESTAMP` parameter spelling onto
/// `SQL_TYPE_TIMESTAMP`.
///
/// The adjacent `SQL_DATE` and `SQL_TIME` values cannot be folded here because
/// ODBC 3.x reuses those numbers for the verbose datetime and interval types.
pub(crate) fn canonical_parameter_sql_type(sql_type: SqlSmallInt) -> SqlSmallInt {
    if sql_type == SQL_TIMESTAMP {
        SQL_TYPE_TIMESTAMP
    } else {
        sql_type
    }
}

/// Fixed byte width of a C value, or `None` when the application sizes the
/// buffer through `BufferLength`.
pub(crate) fn c_type_octet_width(c_type: SqlSmallInt) -> Option<usize> {
    Some(match canonical_c_type(c_type) {
        SQL_C_BIT | SQL_C_TINYINT | SQL_C_STINYINT | SQL_C_UTINYINT => 1,
        SQL_C_SHORT | SQL_C_SSHORT | SQL_C_USHORT => 2,
        SQL_C_LONG | SQL_C_SLONG | SQL_C_ULONG | SQL_C_FLOAT => 4,
        SQL_C_SBIGINT | SQL_C_UBIGINT | SQL_C_DOUBLE => 8,
        SQL_C_GUID => std::mem::size_of::<SqlGuid>(),
        SQL_C_NUMERIC => std::mem::size_of::<SqlNumericStruct>(),
        SQL_C_TYPE_DATE => std::mem::size_of::<SqlDateStruct>(),
        SQL_C_TYPE_TIME => std::mem::size_of::<SqlTimeStruct>(),
        SQL_C_TYPE_TIMESTAMP => std::mem::size_of::<SqlTimestampStruct>(),
        SQL_C_SS_TIME2 => std::mem::size_of::<SqlSsTime2Struct>(),
        SQL_C_SS_TIMESTAMPOFFSET => std::mem::size_of::<SqlSsTimestampoffsetStruct>(),
        _ => return None,
    })
}

/// The C type a parameter binding effectively names, once the SQL type is known.
///
/// `SQL_C_TINYINT` is sign-unknown: `sqlext.h` gives it neither
/// `SQL_SIGNED_OFFSET` nor `SQL_UNSIGNED_OFFSET`. The rule in both directions is
/// that a tinyint-to-tinyint transfer moves the byte unchanged, and every other
/// pairing reads it signed.
///
/// The fetch direction expresses that by copying the byte outright, so no sign
/// is ever chosen. A parameter cannot: `read_param_value` widens every integer
/// C type to `i128`, and widening forces an interpretation. Signed would corrupt
/// the transfer - an application byte of `0xC8` becomes `-56`, which the tinyint
/// column cannot hold - so unsigned is the reading that keeps the widening
/// lossless. The rewrite is how "move the byte unchanged" is spelled in a
/// pipeline that has to widen.
///
/// msodbcsql needs the same rewrite for the same reason, since its parameter
/// path also loads into a widened `Temp` before converting
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcfunc.cpp`, `ParamToSQLType`: "If both are
/// tinyint, change C type to unsigned").
///
/// `SQL_C_STINYINT` is excluded here exactly as it is excluded from the
/// fetch-side byte copy, and any wider `ParameterType` keeps the signed reading.
///
/// No other integer needs this: `tinyint` is the only [unsigned SQL Server
/// integer type](https://learn.microsoft.com/en-us/sql/t-sql/data-types/int-bigint-smallint-and-tinyint-transact-sql),
/// so it is the only one whose same-width pairing reads differently signed.
/// `SQL_C_SHORT` and `SQL_C_LONG` carry no sign offset either, but `smallint`
/// and `int` are signed, so the signed reading is already right for them.
pub(crate) fn effective_param_c_type(c_type: SqlSmallInt, sql_type: SqlSmallInt) -> SqlSmallInt {
    if c_type == SQL_C_TINYINT && sql_type == SQL_TINYINT {
        SQL_C_UTINYINT
    } else {
        c_type
    }
}

/// Whether `sql_type` is one of the UTF-16 character SQL types.
pub(crate) fn is_wide_character_sql_type(sql_type: SqlSmallInt) -> bool {
    matches!(sql_type, SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR)
}

/// Whether `ColumnSize` is legal for `sql_type` on a parameter binding.
///
/// Mirrors msodbcsql's `CheckSqlPrecScale<TRUE>`
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp`), which `SQLBindParameter` runs after
/// the type and conversion checks. Its `CheckSqlPrec` helper rejects both an
/// over-long size and **zero** - `SQL_PREC_UNLIMITED` is 0, so a fixed-length
/// declaration cannot use it. The variable-length types are the exception:
/// against a Yukon-or-later server they only reject an over-long size, because
/// there 0 *is* the `max` spelling.
///
/// msodbcsql clamps instead of failing for an ODBC 2.x application; this driver
/// is 3.x only, so it always reports `HY104`.
pub(crate) fn parameter_column_size_is_valid(sql_type: SqlSmallInt, column_size: usize) -> bool {
    // The lower bound carries the `max` rule: the variable-length types start at
    // `SQL_PREC_UNLIMITED`, the rest at 1 so that 0 reads as a zero-length
    // declaration and is rejected.
    let valid = match sql_type {
        SQL_CHAR | SQL_BINARY => 1..=SQL_PREC_BIGCHARBINARY,
        SQL_WCHAR => 1..=SQL_PREC_NCHAR,
        SQL_VARCHAR | SQL_VARBINARY => SQL_PREC_UNLIMITED..=SQL_PREC_BIGCHARBINARY,
        SQL_WVARCHAR | SQL_SS_XML => SQL_PREC_UNLIMITED..=SQL_PREC_NCHAR,
        SQL_LONGVARCHAR | SQL_LONGVARBINARY => 1..=SQL_PREC_TEXTIMAGE,
        SQL_WLONGVARCHAR => 1..=SQL_PREC_NTEXT,
        SQL_DECIMAL | SQL_NUMERIC => 1..=SQL_PREC_NUMERIC,
        _ => return true,
    };
    valid.contains(&column_size)
}

/// Whether an IPD record's bound `ColumnSize` belongs in `SQL_DESC_PRECISION`
/// rather than `SQL_DESC_LENGTH`.
///
/// This driver stores the two as independent `DescRecord` fields
/// (`get_desc_field.rs` reads each one back directly, with no type-based
/// redirection), unlike msodbcsql's single overloaded `cbColDef`, so
/// `SQLBindParameter`'s IPD auto-population has to pick one. Only the
/// exact-numeric types read `ColumnSize` as a digit count — the same split
/// [`parameter_column_size_is_valid`] already validates against
/// (`1..=SQL_PREC_NUMERIC`); every other type, including the fixed-length
/// numerics where ODBC does not constrain `ColumnSize` at all, is stored as
/// a length.
pub(crate) fn parameter_size_is_precision(sql_type: SqlSmallInt) -> bool {
    matches!(sql_type, SQL_DECIMAL | SQL_NUMERIC)
}

/// Known ODBC C type identifiers in canonical form, including the SQL Server
/// extensions.
///
/// Callers must pass the output of [`canonical_c_type`]: the deprecated
/// `SQL_C_DATE` / `SQL_C_TIME` / `SQL_C_TIMESTAMP` spellings are **not** accepted
/// here, so that exactly one form per type reaches everything downstream.
/// msodbcsql's `IsValidCType` draws the same line in its own canonical form — it
/// bounds the positive range at `fCType <= 32`, rejecting `SQL_C_TYPE_DATE` and
/// friends.
///
/// This is the `HY003` gate only. A C type listed here that the driver cannot
/// yet convert is rejected later with by [`crate::params::conversion_matrix`].
pub(crate) fn is_valid_c_type(c_type: SqlSmallInt) -> bool {
    debug_assert_eq!(
        c_type,
        canonical_c_type(c_type),
        "C type must be canonicalized before validation"
    );
    matches!(
        c_type,
        SQL_C_CHAR
            | SQL_C_WCHAR
            | SQL_C_BIT
            | SQL_C_BINARY
            | SQL_C_GUID
            | SQL_C_NUMERIC
            | SQL_C_STINYINT
            | SQL_C_TINYINT
            | SQL_C_UTINYINT
            | SQL_C_SHORT
            | SQL_C_SSHORT
            | SQL_C_USHORT
            | SQL_C_LONG
            | SQL_C_SLONG
            | SQL_C_ULONG
            | SQL_C_SBIGINT
            | SQL_C_UBIGINT
            | SQL_C_FLOAT
            | SQL_C_DOUBLE
            | SQL_C_TYPE_DATE
            | SQL_C_TYPE_TIME
            | SQL_C_TYPE_TIMESTAMP
            | SQL_C_SS_TIME2
            | SQL_C_SS_TIMESTAMPOFFSET
            | SQL_C_SS_VECTOR
            | SQL_C_DEFAULT
            | SQL_C_INTERVAL_YEAR..=SQL_C_INTERVAL_MINUTE_TO_SECOND
    )
}

/// Whether `c_type` is one of the fixed-width integer C types.
///
/// `SQL_C_BIT` is deliberately excluded: it is a distinct ODBC type with its own
/// 0/1 value model, not an integer.
pub(crate) fn is_integer_c_type(c_type: SqlSmallInt) -> bool {
    matches!(
        c_type,
        SQL_C_STINYINT
            | SQL_C_TINYINT
            | SQL_C_UTINYINT
            | SQL_C_SSHORT
            | SQL_C_SHORT
            | SQL_C_USHORT
            | SQL_C_SLONG
            | SQL_C_LONG
            | SQL_C_ULONG
            | SQL_C_SBIGINT
            | SQL_C_UBIGINT
    )
}

/// How a SQL type identifier is treated at bind time.
///
/// Mirrors the tri-state return of msodbcsql's `IsValidSqlType` (`sqlcprot.h`),
/// which yields `SQL_SUCCESS`, `IDS_S1_C00` (`HYC00`), or `IDS_S1_004`
/// (`HY004`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqlTypeSupport {
    /// A SQL type this driver and SQL Server can carry.
    Supported,
    /// A real ODBC identifier with no SQL Server counterpart (`HYC00`).
    NotImplemented,
    /// Not a known ODBC SQL type identifier (`HY004`).
    Invalid,
}

/// Classifies a `ParameterType` before any conversion check.
pub(crate) fn classify_parameter_sql_type(sql_type: SqlSmallInt) -> SqlTypeSupport {
    match sql_type {
        SQL_CHAR
        | SQL_VARCHAR
        | SQL_LONGVARCHAR
        | SQL_WCHAR
        | SQL_WVARCHAR
        | SQL_WLONGVARCHAR
        | SQL_BINARY
        | SQL_VARBINARY
        | SQL_LONGVARBINARY
        | SQL_DECIMAL
        | SQL_NUMERIC
        | SQL_SMALLINT
        | SQL_INTEGER
        | SQL_BIGINT
        | SQL_TINYINT
        | SQL_BIT
        | SQL_REAL
        | SQL_FLOAT
        | SQL_DOUBLE
        | SQL_GUID
        | SQL_TYPE_DATE
        | SQL_TYPE_TIME
        | SQL_TYPE_TIMESTAMP
        | SQL_SS_TIME2
        | SQL_SS_TIMESTAMPOFFSET
        | SQL_SS_VARIANT
        | SQL_SS_UDT
        | SQL_SS_XML
        | SQL_SS_TABLE
        | SQL_SS_VECTOR => SqlTypeSupport::Supported,
        // SQL Server has no interval type, so no conversion could ever succeed.
        // msodbcsql returns IDS_S1_C00 for this whole range from IsValidSqlType,
        // before IsValidSQLConversion is consulted.
        SQL_INTERVAL_YEAR..=SQL_INTERVAL_MINUTE_TO_SECOND => SqlTypeSupport::NotImplemented,
        _ => SqlTypeSupport::Invalid,
    }
}

/// Resolves `SQL_C_DEFAULT` to the C type implied by `sql_type`.
///
/// Mirrors msodbcsql's `Sql2CDefault` (`sqlcprot.h`), which selects
/// `rgbTRANSTYPE` for a 3.51-or-earlier application and `rgbTRANSTYPE380`
/// otherwise. Per the comment on those tables
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcmisc.cpp`) they differ only in the two SS
/// date/time rows, which default to `SQL_C_BINARY` before ODBC 3.8.
///
/// Two deliberate deviations, both following the ODBC 3.x default-C-type table
/// instead: the wide character types resolve to `SQL_C_WCHAR` and `SQL_GUID` to
/// `SQL_C_GUID`, where msodbcsql resolves both to `SQL_C_CHAR`. That narrow
/// default is an ANSI-transfer artifact with no equivalent here, and resolving
/// UTF-16 input to this driver's UTF-8 `SQL_C_CHAR` would silently corrupt data.
///
/// Every [`SqlTypeSupport::Supported`] type has a mapping; `None` means the
/// caller passed a type that should already have been rejected.
pub(crate) fn resolve_default_c_type(
    sql_type: SqlSmallInt,
    odbc_version: OdbcVersion,
) -> Option<SqlSmallInt> {
    let is_3_80 = odbc_version == OdbcVersion::Odbc3_80;
    Some(match sql_type {
        SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR => SQL_C_CHAR,
        SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR => SQL_C_WCHAR,
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => SQL_C_BINARY,
        SQL_DECIMAL | SQL_NUMERIC => SQL_C_CHAR,
        SQL_BIT => SQL_C_BIT,
        SQL_TINYINT => SQL_C_UTINYINT,
        SQL_SMALLINT => SQL_C_SSHORT,
        SQL_INTEGER => SQL_C_SLONG,
        SQL_BIGINT => SQL_C_SBIGINT,
        SQL_REAL => SQL_C_FLOAT,
        SQL_FLOAT | SQL_DOUBLE => SQL_C_DOUBLE,
        SQL_GUID => SQL_C_GUID,
        SQL_TYPE_DATE => SQL_C_TYPE_DATE,
        SQL_TYPE_TIME => SQL_C_TYPE_TIME,
        SQL_TYPE_TIMESTAMP => SQL_C_TYPE_TIMESTAMP,
        SQL_SS_TIME2 if is_3_80 => SQL_C_SS_TIME2,
        SQL_SS_TIME2 => SQL_C_BINARY,
        SQL_SS_TIMESTAMPOFFSET if is_3_80 => SQL_C_SS_TIMESTAMPOFFSET,
        SQL_SS_TIMESTAMPOFFSET => SQL_C_BINARY,
        SQL_SS_VARIANT => SQL_C_CHAR,
        SQL_SS_UDT | SQL_SS_TABLE => SQL_C_BINARY,
        SQL_SS_XML => SQL_C_WCHAR,
        SQL_SS_VECTOR => SQL_C_SS_VECTOR,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_C_TIME;

    #[test]
    fn real_c_types_are_valid_even_when_unconvertible() {
        // These are legal ODBC C types the driver cannot convert yet; they must
        // pass the HY003 gate so the conversion check can report error instead.
        for c_type in [
            SQL_C_SLONG,
            SQL_C_SBIGINT,
            SQL_C_UTINYINT,
            SQL_C_GUID,
            SQL_C_BINARY,
            SQL_C_TYPE_TIMESTAMP,
            SQL_C_INTERVAL_YEAR,
            SQL_C_INTERVAL_MINUTE_TO_SECOND,
        ] {
            assert!(is_valid_c_type(c_type), "{c_type} should be a valid C type");
        }
    }

    #[test]
    fn unknown_c_type_is_invalid() {
        assert!(!is_valid_c_type(9999));
    }

    #[test]
    fn unknown_sql_type_is_invalid() {
        assert_eq!(
            classify_parameter_sql_type(SQL_VARCHAR),
            SqlTypeSupport::Supported
        );
        assert_eq!(classify_parameter_sql_type(9999), SqlTypeSupport::Invalid);
    }

    /// Keeps the two halves of the type model in step: `bind_param` relies on
    /// every `Supported` type having a default C type, so a new row in one
    /// function without the other must fail here rather than at runtime.
    #[test]
    fn every_supported_sql_type_has_a_default_c_type() {
        for version in [
            OdbcVersion::Unset,
            OdbcVersion::Odbc2,
            OdbcVersion::Odbc3,
            OdbcVersion::Odbc3_80,
        ] {
            for sql_type in i16::MIN..=i16::MAX {
                if classify_parameter_sql_type(sql_type) != SqlTypeSupport::Supported {
                    continue;
                }
                assert!(
                    resolve_default_c_type(sql_type, version).is_some(),
                    "SQL type {sql_type} is Supported but has no default C type at {version:?}"
                );
            }
        }
    }

    /// The other half of that coupling, and the one with a buffer overrun on
    /// the far side of it.
    ///
    /// Both `SQL_C_DEFAULT` resolvers refuse to resolve a fixed-width target
    /// the caller's buffer cannot hold, and both ask [`element_stride`] how
    /// wide that target is by calling it with `buffer_length` 0
    /// (`get_data::resolve_default_target`,
    /// `fetch_scroll::resolve_default_bindings`). `element_stride`'s catch-all
    /// is `_ => buffer_length`, which is 0 there — so a fixed-width C type
    /// *missing* from its match reports width 0, the `fixed_width > 0` guard
    /// skips it, and the resolver hands a fixed-width target to a buffer the
    /// application may have sized smaller.
    ///
    /// Nothing else fails in that case: `element_stride`'s own test checks a
    /// hand-picked subset, so dropping a type from the match leaves every
    /// behavioral test green. This closes it over the whole mapping rather than
    /// a list that can go stale — a future row emitting a fixed-width C type
    /// `element_stride` does not size fails here instead of at an application's
    /// buffer.
    ///
    /// `SQL_C_NUMERIC` is the live example of why this is not hypothetical: it
    /// is fixed-width and absent from `element_stride`, and is safe today only
    /// because `SQL_DECIMAL` / `SQL_NUMERIC` map to `SQL_C_CHAR`. That is a
    /// property of the mapping, not of the guard.
    ///
    /// Raised by Saurabh in review of PR #481.
    #[test]
    fn every_default_c_type_has_a_width_or_is_app_sized() {
        use crate::api::fetch_scroll::element_stride;

        // The targets ODBC sizes from the application's `BufferLength`, for
        // which a 0 width is the correct answer rather than a gap.
        const APP_SIZED: &[SqlSmallInt] = &[SQL_C_CHAR, SQL_C_WCHAR, SQL_C_BINARY, SQL_C_SS_VECTOR];

        for version in [
            OdbcVersion::Unset,
            OdbcVersion::Odbc2,
            OdbcVersion::Odbc3,
            OdbcVersion::Odbc3_80,
        ] {
            for sql_type in i16::MIN..=i16::MAX {
                let Some(c_type) = resolve_default_c_type(sql_type, version) else {
                    continue;
                };
                assert!(
                    APP_SIZED.contains(&c_type) || element_stride(c_type, 0) > 0,
                    "SQL_C_DEFAULT on SQL type {sql_type} at {version:?} resolves to C type \
                     {c_type}, which element_stride reports as width 0 without being \
                     application-sized; the resolvers' narrow-buffer guard cannot see it"
                );
            }
        }
    }

    #[test]
    fn default_c_type_follows_the_sql_type() {
        let v = OdbcVersion::Odbc3_80;
        assert_eq!(resolve_default_c_type(SQL_VARCHAR, v), Some(SQL_C_CHAR));
        assert_eq!(resolve_default_c_type(SQL_LONGVARCHAR, v), Some(SQL_C_CHAR));
        assert_eq!(resolve_default_c_type(SQL_WVARCHAR, v), Some(SQL_C_WCHAR));
        assert_eq!(resolve_default_c_type(SQL_VARBINARY, v), Some(SQL_C_BINARY));
        assert_eq!(resolve_default_c_type(SQL_DECIMAL, v), Some(SQL_C_CHAR));
        assert_eq!(resolve_default_c_type(SQL_BIT, v), Some(SQL_C_BIT));
        assert_eq!(resolve_default_c_type(SQL_TINYINT, v), Some(SQL_C_UTINYINT));
        assert_eq!(resolve_default_c_type(SQL_SMALLINT, v), Some(SQL_C_SSHORT));
        assert_eq!(resolve_default_c_type(SQL_INTEGER, v), Some(SQL_C_SLONG));
        assert_eq!(resolve_default_c_type(SQL_BIGINT, v), Some(SQL_C_SBIGINT));
        assert_eq!(resolve_default_c_type(SQL_REAL, v), Some(SQL_C_FLOAT));
        assert_eq!(resolve_default_c_type(SQL_DOUBLE, v), Some(SQL_C_DOUBLE));
        assert_eq!(resolve_default_c_type(SQL_GUID, v), Some(SQL_C_GUID));
        assert_eq!(
            resolve_default_c_type(SQL_TYPE_TIMESTAMP, v),
            Some(SQL_C_TYPE_TIMESTAMP)
        );
        assert_eq!(
            resolve_default_c_type(SQL_SS_TIMESTAMPOFFSET, v),
            Some(SQL_C_SS_TIMESTAMPOFFSET)
        );
        assert_eq!(
            resolve_default_c_type(SQL_SS_VECTOR, v),
            Some(SQL_C_SS_VECTOR)
        );
    }

    /// msodbcsql's `Sql2CDefault` picks `rgbTRANSTYPE` below ODBC 3.8, whose only
    /// difference is that the two SS date/time rows default to `SQL_C_BINARY`.
    #[test]
    fn ss_datetime_defaults_depend_on_the_odbc_version() {
        for older in [OdbcVersion::Unset, OdbcVersion::Odbc2, OdbcVersion::Odbc3] {
            assert_eq!(
                resolve_default_c_type(SQL_SS_TIME2, older),
                Some(SQL_C_BINARY)
            );
            assert_eq!(
                resolve_default_c_type(SQL_SS_TIMESTAMPOFFSET, older),
                Some(SQL_C_BINARY)
            );
        }
        assert_eq!(
            resolve_default_c_type(SQL_SS_TIME2, OdbcVersion::Odbc3_80),
            Some(SQL_C_SS_TIME2)
        );
        assert_eq!(
            resolve_default_c_type(SQL_SS_TIMESTAMPOFFSET, OdbcVersion::Odbc3_80),
            Some(SQL_C_SS_TIMESTAMPOFFSET)
        );
    }

    #[test]
    fn deprecated_datetime_c_types_fold_onto_the_3x_forms() {
        assert_eq!(canonical_c_type(SQL_C_DATE), SQL_C_TYPE_DATE);
        assert_eq!(canonical_c_type(SQL_C_TIME), SQL_C_TYPE_TIME);
        assert_eq!(canonical_c_type(SQL_C_TIMESTAMP), SQL_C_TYPE_TIMESTAMP);
        // Already canonical, and unrelated types, pass through untouched.
        assert_eq!(canonical_c_type(SQL_C_TYPE_DATE), SQL_C_TYPE_DATE);
        assert_eq!(canonical_c_type(SQL_C_CHAR), SQL_C_CHAR);
        assert_eq!(canonical_c_type(SQL_C_SLONG), SQL_C_SLONG);
        // Folding is what makes the deprecated spellings pass the HY003 gate;
        // is_valid_c_type itself only accepts the canonical form.
        assert!(is_valid_c_type(canonical_c_type(SQL_C_TIMESTAMP)));
    }

    #[test]
    fn only_unambiguous_legacy_timestamp_sql_type_is_canonicalized() {
        assert_eq!(
            canonical_parameter_sql_type(SQL_TIMESTAMP),
            SQL_TYPE_TIMESTAMP
        );
        assert_eq!(canonical_parameter_sql_type(9), 9);
        assert_eq!(canonical_parameter_sql_type(10), 10);
    }

    #[test]
    fn interval_sql_types_are_not_implemented() {
        for sql_type in [
            SQL_INTERVAL_YEAR,
            SQL_INTERVAL_MINUTE_TO_SECOND,
            SQL_INTERVAL_YEAR + 1,
        ] {
            assert_eq!(
                classify_parameter_sql_type(sql_type),
                SqlTypeSupport::NotImplemented,
                "{sql_type} should be HYC00, not a conversion failure"
            );
        }
    }

    /// Only the variable-length types read `ColumnSize` 0 as the `max` spelling;
    /// everywhere else msodbcsql's `CheckSqlPrec` rejects it, because
    /// `SQL_PREC_UNLIMITED` is 0.
    #[test]
    fn zero_column_size_is_max_only_for_the_variable_length_types() {
        for sql_type in [SQL_VARCHAR, SQL_WVARCHAR, SQL_VARBINARY, SQL_SS_XML] {
            assert!(
                parameter_column_size_is_valid(sql_type, 0),
                "{sql_type} should read 0 as max"
            );
        }
        for sql_type in [
            SQL_CHAR,
            SQL_WCHAR,
            SQL_BINARY,
            SQL_LONGVARCHAR,
            SQL_WLONGVARCHAR,
            SQL_DECIMAL,
        ] {
            assert!(
                !parameter_column_size_is_valid(sql_type, 0),
                "{sql_type} should reject 0"
            );
        }
    }

    #[test]
    fn column_size_limits_follow_the_sql_type() {
        assert!(parameter_column_size_is_valid(SQL_CHAR, 8000));
        assert!(!parameter_column_size_is_valid(SQL_CHAR, 8001));
        assert!(parameter_column_size_is_valid(SQL_WCHAR, 4000));
        assert!(!parameter_column_size_is_valid(SQL_WCHAR, 4001));
        assert!(parameter_column_size_is_valid(SQL_VARCHAR, 8000));
        assert!(!parameter_column_size_is_valid(SQL_VARCHAR, 8001));
        assert!(parameter_column_size_is_valid(SQL_SS_XML, 4000));
        assert!(!parameter_column_size_is_valid(SQL_SS_XML, 4001));
        assert!(parameter_column_size_is_valid(SQL_DECIMAL, 38));
        assert!(!parameter_column_size_is_valid(SQL_DECIMAL, 39));
        // The `long` variants bound at the `text`/`ntext` sizes, and `ntext` is
        // half of `text` because its limit counts characters, not bytes.
        assert!(parameter_column_size_is_valid(
            SQL_LONGVARCHAR,
            SQL_PREC_TEXTIMAGE
        ));
        assert!(!parameter_column_size_is_valid(
            SQL_LONGVARCHAR,
            SQL_PREC_TEXTIMAGE + 1
        ));
        assert!(parameter_column_size_is_valid(
            SQL_LONGVARBINARY,
            SQL_PREC_TEXTIMAGE
        ));
        assert!(parameter_column_size_is_valid(
            SQL_WLONGVARCHAR,
            SQL_PREC_NTEXT
        ));
        assert!(!parameter_column_size_is_valid(
            SQL_WLONGVARCHAR,
            SQL_PREC_NTEXT + 1
        ));
        // The integer, bit and guid types have a case in CheckSqlPrecScale that
        // breaks without validating, so nothing is checked for them.
        for sql_type in [SQL_INTEGER, SQL_SMALLINT, SQL_TINYINT, SQL_BIT, SQL_GUID] {
            assert!(parameter_column_size_is_valid(sql_type, 0));
            assert!(parameter_column_size_is_valid(sql_type, 999_999));
        }
    }

    #[test]
    fn parameter_size_is_precision_only_for_exact_numerics() {
        assert!(parameter_size_is_precision(SQL_DECIMAL));
        assert!(parameter_size_is_precision(SQL_NUMERIC));
        for sql_type in [
            SQL_CHAR,
            SQL_VARCHAR,
            SQL_WCHAR,
            SQL_BINARY,
            SQL_INTEGER,
            SQL_FLOAT,
            SQL_GUID,
        ] {
            assert!(
                !parameter_size_is_precision(sql_type),
                "{sql_type} should be stored as a length"
            );
        }
    }
}
