// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Which C type → SQL type parameter conversions the driver can perform.
//!
//! Table-driven, in the same shape as msodbcsql's `fValidConversion` matrix
//! (`Sql/Ntdbms/sqlncli/odbc/sqlcmisc.cpp`), which is indexed by C type and
//! yields the set of legal SQL types. The semantics differ: that matrix answers
//! "is this pairing legal?", this one answers "is this pairing implemented?".
//! The rows here list only the pairings this driver implements today; rows and
//! entries are added as each conversion lands, so a pairing accepted at bind
//! time is always one the execute path can actually convert.
//!
//! Parameter-side only, matching msodbcsql: it consults `IsValidSQLConversion`
//! where both types are known up front (`SQLBindParameter`, output-parameter
//! retrieval, BCP), but `SQLBindCol` / `SQLGetData` cannot — a column's SQL type
//! may be unknown until after execute — so the fetch direction reports the same
//! `07006` from inside its converters (`ConvError::Restricted`) instead.

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_C_BINARY, SQL_C_CHAR, SQL_C_DEFAULT, SQL_C_WCHAR, SQL_CHAR,
    SQL_INTEGER, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_SMALLINT, SQL_TINYINT, SQL_VARBINARY,
    SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlSmallInt,
};
use crate::api::type_rules::is_integer_c_type;

const CHARACTER_SQL_TARGETS: &[SqlSmallInt] = &[
    SQL_CHAR,
    SQL_VARCHAR,
    SQL_LONGVARCHAR,
    SQL_WCHAR,
    SQL_WVARCHAR,
    SQL_WLONGVARCHAR,
];

const BINARY_SQL_TARGETS: &[SqlSmallInt] = &[SQL_BINARY, SQL_VARBINARY, SQL_LONGVARBINARY];

/// Width is not part of legality: a value that does not fit the target is a
/// runtime `22003`, not a rejected binding, so `SQL_TINYINT` stays reachable
/// from every integer and character C type.
const INTEGER_SQL_TARGETS: &[SqlSmallInt] = &[SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT];

/// Whether the driver can convert a `c_type` application buffer into `sql_type`
/// for an input parameter.
///
/// TODO: once every row of msodbcsql's `fValidConversion` is covered here, this
/// stops being a progress list and becomes a legality table — at which point the
/// caller must report `07006` instead of `HYC00`, because a missing entry then
/// means the conversion is genuinely illegal rather than merely unbuilt.
pub(crate) fn is_supported_conversion(c_type: SqlSmallInt, sql_type: SqlSmallInt) -> bool {
    debug_assert_ne!(
        c_type, SQL_C_DEFAULT,
        "SQL_C_DEFAULT must be resolved before consulting the conversion matrix"
    );
    let targets: &[&[SqlSmallInt]] = match c_type {
        SQL_C_CHAR | SQL_C_WCHAR => &[CHARACTER_SQL_TARGETS, INTEGER_SQL_TARGETS],
        SQL_C_BINARY => &[BINARY_SQL_TARGETS],
        _ if is_integer_c_type(c_type) => &[INTEGER_SQL_TARGETS, CHARACTER_SQL_TARGETS],
        _ => return false,
    };
    targets.iter().any(|group| group.contains(&sql_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_LONG, SQL_C_SBIGINT, SQL_C_SHORT, SQL_C_SLONG, SQL_C_SSHORT, SQL_C_STINYINT,
        SQL_C_TINYINT, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_GUID,
    };

    /// Cross-family pairings transcode UTF-8 <-> UTF-16 rather than being
    /// rejected, so every character C type reaches every character SQL type.
    #[test]
    fn every_character_c_type_reaches_every_character_sql_type() {
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
            for sql_type in [
                SQL_CHAR,
                SQL_VARCHAR,
                SQL_LONGVARCHAR,
                SQL_WCHAR,
                SQL_WVARCHAR,
                SQL_WLONGVARCHAR,
            ] {
                assert!(
                    is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should be supported"
                );
            }
        }
    }

    #[test]
    fn binary_c_type_reaches_only_the_binary_sql_types() {
        for sql_type in [SQL_BINARY, SQL_VARBINARY, SQL_LONGVARBINARY] {
            assert!(is_supported_conversion(SQL_C_BINARY, sql_type));
        }
        assert!(!is_supported_conversion(SQL_C_BINARY, SQL_VARCHAR));
    }

    /// A character buffer parses a numeric literal, so it reaches the integer
    /// SQL types as well as the character ones.
    #[test]
    fn every_character_c_type_reaches_every_integer_sql_type() {
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
            for sql_type in [SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT] {
                assert!(
                    is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should be supported"
                );
            }
        }
    }

    /// An integer buffer formats itself base 10, so it reaches the character SQL
    /// types as well as the integer ones.
    #[test]
    fn every_integer_c_type_reaches_every_character_sql_type() {
        for c_type in [SQL_C_STINYINT, SQL_C_USHORT, SQL_C_SLONG, SQL_C_UBIGINT] {
            for sql_type in [
                SQL_CHAR,
                SQL_VARCHAR,
                SQL_LONGVARCHAR,
                SQL_WCHAR,
                SQL_WVARCHAR,
                SQL_WLONGVARCHAR,
            ] {
                assert!(
                    is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should be supported"
                );
            }
        }
    }

    /// The binary row is not part of the character/integer composition, so it
    /// gains nothing from the cross-family rows.
    #[test]
    fn binary_stays_outside_the_cross_family_rows() {
        assert!(!is_supported_conversion(SQL_C_BINARY, SQL_INTEGER));
        assert!(!is_supported_conversion(SQL_C_CHAR, SQL_VARBINARY));
        assert!(!is_supported_conversion(SQL_C_SLONG, SQL_VARBINARY));
    }

    #[test]
    fn every_integer_c_type_reaches_every_integer_sql_type() {
        for c_type in [
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
        ] {
            for sql_type in [SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT] {
                assert!(
                    is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should be supported"
                );
            }
        }
    }

    #[test]
    fn c_types_without_a_row_are_unsupported() {
        assert!(!is_supported_conversion(SQL_C_SLONG, SQL_GUID));
        assert!(!is_supported_conversion(SQL_C_CHAR, SQL_GUID));
    }
}
