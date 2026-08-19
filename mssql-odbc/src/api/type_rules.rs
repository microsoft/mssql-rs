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
    SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR,
    SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlSmallInt,
};
use crate::handles::OdbcVersion;

/// Offset between the ODBC 2.x (`9`..`11`) and 3.x (`91`..`93`) date/time ids.
const ODBC2_DATETIME_OFFSET: SqlSmallInt = SQL_C_TYPE_DATE - SQL_C_DATE;

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
/// yet convert is rejected later with `07006`, matching msodbcsql, which treats
/// an unconvertible-but-real C type as a restricted conversion rather than an
/// out-of-range program type.
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
        // pass the HY003 gate so the conversion check can report 07006 instead.
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
}
