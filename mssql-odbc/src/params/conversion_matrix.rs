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
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DEFAULT,
    SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID, SQL_C_NUMERIC, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET,
    SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP, SQL_C_WCHAR, SQL_CHAR, SQL_DECIMAL,
    SQL_DOUBLE, SQL_FLOAT, SQL_GUID, SQL_INTEGER, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NUMERIC,
    SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SS_UDT, SQL_SS_VARIANT,
    SQL_SS_XML, SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY,
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

const DECIMAL_SQL_TARGETS: &[SqlSmallInt] = &[SQL_DECIMAL, SQL_NUMERIC];

const TEMPORAL_SQL_TARGETS: &[SqlSmallInt] = &[
    SQL_TYPE_DATE,
    SQL_TYPE_TIME,
    SQL_SS_TIME2,
    SQL_TYPE_TIMESTAMP,
    SQL_SS_TIMESTAMPOFFSET,
];

/// `xml` takes a character payload but declares its own wire type, so it is
/// listed apart from `CHARACTER_SQL_TARGETS`.
const CHARACTER_PAYLOAD_SQL_TARGETS: &[SqlSmallInt] = &[SQL_SS_XML];

/// A CLR UDT takes its payload untouched from a `SQL_C_BINARY` buffer, and its
/// identity from the IPD's `SQL_CA_SS_UDT_*` fields.
///
/// `fValidConversion` also admits `SQL_C_CHAR` and `SQL_C_WCHAR`, but those
/// rows are not built yet rather than deliberately refused: msodbcsql does not
/// pass a character buffer through, it hex-decodes it two characters to the
/// byte. `rgbTRANSTYPE*` gives `SQL_UDT_MAPPED` a `SQL_C_BINARY` transfer type
/// (`sqlcmisc.cpp:67`, `:178`, `:217`), so `ConvertLongData` misses its
/// pass-through guard (`sqlccnvt.cpp:874-877`) and lands on the branch
/// commented "CHAR/WCHAR ->binary (2 chars are converted to only one single
/// binary byte)" (`sqlccnvt.cpp:1014-1016`); the `cbMax*2` length checks at
/// `sqlcfunc.cpp:3048-3063` corroborate the ratio. Admitting them while
/// sending the buffer verbatim would put different bytes on the wire than the
/// reference for the same binding. Source reading only - the measurement that
/// decides whether to implement the decode is named in `bound_param_to_value`'s
/// UDT arm. AB#48248.
const UDT_SQL_TARGETS: &[SqlSmallInt] = &[SQL_SS_UDT];

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
        SQL_C_CHAR => &[
            CHARACTER_SQL_TARGETS,
            INTEGER_SQL_TARGETS,
            DECIMAL_SQL_TARGETS,
            TEMPORAL_SQL_TARGETS,
            CHARACTER_PAYLOAD_SQL_TARGETS,
            &[SQL_SS_VARIANT],
        ],
        SQL_C_WCHAR => &[
            CHARACTER_SQL_TARGETS,
            INTEGER_SQL_TARGETS,
            DECIMAL_SQL_TARGETS,
            TEMPORAL_SQL_TARGETS,
            CHARACTER_PAYLOAD_SQL_TARGETS,
            &[SQL_SS_VARIANT],
        ],
        SQL_C_BINARY => &[BINARY_SQL_TARGETS, &[SQL_SS_VARIANT], UDT_SQL_TARGETS],
        SQL_C_BIT => &[&[SQL_BIT]],
        SQL_C_FLOAT | SQL_C_DOUBLE => &[&[SQL_REAL, SQL_FLOAT, SQL_DOUBLE]],
        SQL_C_GUID => &[&[SQL_GUID]],
        SQL_C_NUMERIC => &[DECIMAL_SQL_TARGETS],
        SQL_C_TYPE_DATE => &[&[SQL_TYPE_DATE]],
        // `time` and its SS spelling are one wire type, so both C spellings
        // reach both SQL spellings.
        SQL_C_TYPE_TIME | SQL_C_SS_TIME2 => &[&[SQL_TYPE_TIME, SQL_SS_TIME2]],
        SQL_C_TYPE_TIMESTAMP => &[&[SQL_TYPE_TIMESTAMP]],
        SQL_C_SS_TIMESTAMPOFFSET => &[&[SQL_SS_TIMESTAMPOFFSET]],
        _ if is_integer_c_type(c_type) => &[INTEGER_SQL_TARGETS, CHARACTER_SQL_TARGETS],
        _ => return false,
    };
    targets.iter().any(|group| group.contains(&sql_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_LONG, SQL_C_SBIGINT, SQL_C_SHORT, SQL_C_SLONG, SQL_C_SS_VECTOR, SQL_C_SSHORT,
        SQL_C_STINYINT, SQL_C_TINYINT, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT,
        SQL_GUID, SQL_SS_TABLE, SQL_SS_UDT, SQL_SS_VECTOR,
    };
    use crate::api::type_rules::{SqlTypeSupport, classify_parameter_sql_type};

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

    /// `sql_variant` joins the binary targets because msodbcsql derives a
    /// variant's inner type from the C type (`CTypeToSqlType`, `sqlcprot.h`),
    /// which answers `varbinary` for `SQL_C_BINARY`.
    #[test]
    fn binary_c_type_reaches_the_binary_sql_types_and_variant() {
        for sql_type in [SQL_BINARY, SQL_VARBINARY, SQL_LONGVARBINARY, SQL_SS_VARIANT] {
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
        // `SQL_C_SS_VECTOR` is a real ODBC C type with no row yet (AB#47790).
        for sql_type in every_supported_sql_type() {
            assert!(
                !is_supported_conversion(SQL_C_SS_VECTOR, sql_type),
                "SQL_C_SS_VECTOR should reach nothing, but reached {sql_type}"
            );
        }
    }

    /// Every SQL type a `ParameterType` may name, derived rather than listed so
    /// a newly supported type is automatically checked against every row below.
    fn every_supported_sql_type() -> Vec<SqlSmallInt> {
        (-160..=120)
            .filter(|sql_type| {
                matches!(
                    classify_parameter_sql_type(*sql_type),
                    SqlTypeSupport::Supported
                )
            })
            .collect()
    }

    /// mssql-python binds a `datetime.time` as isoformat text against
    /// `SQL_TYPE_TIME`, so the character rows have to reach the temporal
    /// targets or the whole insert fails at bind time (AB#47851).
    #[test]
    fn a_character_c_type_reaches_the_temporal_sql_types() {
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
            for sql_type in [
                SQL_TYPE_DATE,
                SQL_TYPE_TIME,
                SQL_SS_TIME2,
                SQL_TYPE_TIMESTAMP,
                SQL_SS_TIMESTAMPOFFSET,
            ] {
                assert!(
                    is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should be supported"
                );
            }
        }
    }

    /// The scalar rows, each with the exact set of SQL types it may reach.
    ///
    /// Asserted in both directions: a row that quietly widens is a binding
    /// accepted here and rejected at execute, which is the one thing this table
    /// exists to prevent.
    #[test]
    fn the_scalar_rows_reach_exactly_their_own_sql_types() {
        let rows: &[(SqlSmallInt, &[SqlSmallInt])] = &[
            (SQL_C_BIT, &[SQL_BIT]),
            (SQL_C_FLOAT, &[SQL_REAL, SQL_FLOAT, SQL_DOUBLE]),
            (SQL_C_DOUBLE, &[SQL_REAL, SQL_FLOAT, SQL_DOUBLE]),
            (SQL_C_GUID, &[SQL_GUID]),
            (SQL_C_NUMERIC, &[SQL_DECIMAL, SQL_NUMERIC]),
            (SQL_C_TYPE_DATE, &[SQL_TYPE_DATE]),
            (SQL_C_TYPE_TIME, &[SQL_TYPE_TIME, SQL_SS_TIME2]),
            (SQL_C_SS_TIME2, &[SQL_TYPE_TIME, SQL_SS_TIME2]),
            (SQL_C_TYPE_TIMESTAMP, &[SQL_TYPE_TIMESTAMP]),
            (SQL_C_SS_TIMESTAMPOFFSET, &[SQL_SS_TIMESTAMPOFFSET]),
        ];
        for (c_type, expected) in rows {
            for sql_type in every_supported_sql_type() {
                assert_eq!(
                    is_supported_conversion(*c_type, sql_type),
                    expected.contains(&sql_type),
                    "{c_type} -> {sql_type}"
                );
            }
        }
    }

    /// Character C types reach the non-character payloads that can be serialized.
    #[test]
    fn a_character_c_type_reaches_the_types_that_default_to_one() {
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR] {
            for sql_type in [SQL_DECIMAL, SQL_NUMERIC, SQL_SS_XML, SQL_SS_VARIANT] {
                assert!(
                    is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should be supported"
                );
            }
        }
    }

    /// Unrelated scalar C types do not reach these payloads. `SQL_C_BINARY` is
    /// asserted apart because it reaches `sql_variant` but none of the rest.
    ///
    /// The `sql_variant` half is narrower than msodbcsql, not settled intent:
    /// `IsValidSQLConversion`'s `case SQL_VARIANT_MAPPED:` (`sqlcprot.h`)
    /// rejects only `SQL_C_DATE`/`SQL_C_TIME` pre-Katmai and then falls through
    /// to `fValidConversion`, whose `CHARCONVERSION` sets the variant bit
    /// (`sqlcmisc.cpp:497-507`) and which every `NUMERICCONVERSION`-based mask
    /// inherits - so the reference admits `SQL_C_BIT`, `SQL_C_GUID`, the
    /// integers, the floats, `NUMERIC` and the temporals too. Widening the row
    /// needs matching conversion arms and e2e parity coverage, tracked in
    /// AB#48453. `SQL_DECIMAL`/`SQL_NUMERIC`/`SQL_SS_XML` are unaffected.
    #[test]
    fn unrelated_c_types_do_not_reach_decimal_xml_or_variant() {
        for c_type in [
            SQL_C_BIT,
            SQL_C_DOUBLE,
            SQL_C_GUID,
            SQL_C_TYPE_TIMESTAMP,
            SQL_C_SLONG,
        ] {
            for sql_type in [SQL_DECIMAL, SQL_NUMERIC, SQL_SS_XML, SQL_SS_VARIANT] {
                assert!(
                    !is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should not be supported"
                );
            }
        }
        for sql_type in [SQL_DECIMAL, SQL_NUMERIC, SQL_SS_XML] {
            assert!(
                !is_supported_conversion(SQL_C_BINARY, sql_type),
                "SQL_C_BINARY -> {sql_type} should not be supported"
            );
        }
    }

    /// A UDT takes its payload untouched from a `SQL_C_BINARY` buffer, and
    /// nothing else reaches it today. A scalar C type has no serialized form
    /// the server could interpret as the UDT's own; the character rows are a
    /// different case - `fValidConversion` admits them, but msodbcsql
    /// hex-decodes rather than passing through, so they stay unbuilt until
    /// that is measured (see `UDT_SQL_TARGETS`, AB#48248).
    #[test]
    fn only_a_binary_buffer_reaches_udt_today() {
        assert!(is_supported_conversion(SQL_C_BINARY, SQL_SS_UDT));
        for c_type in [
            SQL_C_CHAR,
            SQL_C_WCHAR,
            SQL_C_BIT,
            SQL_C_DOUBLE,
            SQL_C_GUID,
            SQL_C_SLONG,
            SQL_C_NUMERIC,
            SQL_C_TYPE_TIMESTAMP,
        ] {
            assert!(!is_supported_conversion(c_type, SQL_SS_UDT), "{c_type}");
        }
    }

    /// The rows still deferred: `vector` is P9g (AB#48326) and TVP is a feature
    /// of its own. Pinned so adding one is a deliberate edit here rather than a
    /// side effect of widening something else.
    #[test]
    fn the_deferred_sql_types_are_reached_by_nothing() {
        let c_types = [
            SQL_C_CHAR,
            SQL_C_WCHAR,
            SQL_C_BINARY,
            SQL_C_BIT,
            SQL_C_DOUBLE,
            SQL_C_GUID,
            SQL_C_SLONG,
            SQL_C_TYPE_TIMESTAMP,
        ];
        for c_type in c_types {
            for sql_type in [SQL_SS_VECTOR, SQL_SS_TABLE] {
                assert!(
                    !is_supported_conversion(c_type, sql_type),
                    "{c_type} -> {sql_type} should not be supported"
                );
            }
        }
    }
}
