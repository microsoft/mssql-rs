// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Which C type → SQL type parameter conversions the driver can perform.
//!
//! Table-driven, mirroring msodbcsql's `fValidConversion` matrix
//! (`Sql/Ntdbms/sqlncli/odbc/sqlcmisc.cpp`), which is indexed by C type and
//! yields the set of legal SQL types. The rows here list only the pairings this
//! driver implements today; rows and entries are added as each conversion
//! lands, so a pairing accepted at bind time is always one the execute path can
//! actually convert.
//!
//! Anything absent is rejected with `07006` at bind time rather than failing
//! later at execute.
//!
//! Parameter-side only, matching msodbcsql: it consults `IsValidSQLConversion`
//! where both types are known up front (`SQLBindParameter`, output-parameter
//! retrieval, BCP), but `SQLBindCol` / `SQLGetData` cannot — a column's SQL type
//! may be unknown until after execute — so the fetch direction reports the same
//! `07006` from inside its converters (`ConvError::Restricted`) instead.

use crate::api::odbc_types::{
    SQL_C_CHAR, SQL_C_WCHAR, SQL_CHAR, SQL_LONGVARCHAR, SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR,
    SQL_WVARCHAR, SqlSmallInt,
};

/// SQL types a `SQL_C_CHAR` buffer can be converted to.
const CHAR_C_TARGETS: &[SqlSmallInt] = &[SQL_CHAR, SQL_VARCHAR, SQL_LONGVARCHAR];

/// SQL types a `SQL_C_WCHAR` buffer can be converted to.
const WCHAR_C_TARGETS: &[SqlSmallInt] = &[SQL_WCHAR, SQL_WVARCHAR, SQL_WLONGVARCHAR];

/// Whether the driver can convert a `c_type` application buffer into `sql_type`
/// for an input parameter.
pub(crate) fn is_supported_conversion(c_type: SqlSmallInt, sql_type: SqlSmallInt) -> bool {
    let targets: &[SqlSmallInt] = match c_type {
        SQL_C_CHAR => CHAR_C_TARGETS,
        SQL_C_WCHAR => WCHAR_C_TARGETS,
        _ => return false,
    };
    targets.contains(&sql_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_SLONG, SQL_GUID, SQL_INTEGER};

    #[test]
    fn narrow_character_conversions_are_supported() {
        for sql_type in [SQL_CHAR, SQL_VARCHAR, SQL_LONGVARCHAR] {
            assert!(is_supported_conversion(SQL_C_CHAR, sql_type));
        }
    }

    #[test]
    fn wide_character_conversions_are_supported() {
        for sql_type in [SQL_WCHAR, SQL_WVARCHAR, SQL_WLONGVARCHAR] {
            assert!(is_supported_conversion(SQL_C_WCHAR, sql_type));
        }
    }

    #[test]
    fn cross_family_character_conversions_are_unsupported() {
        assert!(!is_supported_conversion(SQL_C_CHAR, SQL_WVARCHAR));
        assert!(!is_supported_conversion(SQL_C_WCHAR, SQL_VARCHAR));
    }

    #[test]
    fn numeric_conversions_are_unsupported() {
        assert!(!is_supported_conversion(SQL_C_CHAR, SQL_INTEGER));
        assert!(!is_supported_conversion(SQL_C_SLONG, SQL_INTEGER));
    }

    #[test]
    fn c_types_without_a_row_are_unsupported() {
        assert!(!is_supported_conversion(SQL_C_SLONG, SQL_GUID));
    }
}
