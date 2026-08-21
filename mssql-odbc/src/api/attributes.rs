// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Attribute identifier metadata shared by the `SQLSet*Attr` / `SQLGet*Attr`
//! entry points.
//!
//! ODBC draws a sharp line between two failures that callers handle very
//! differently:
//!
//! * `HY092` - *Invalid attribute/option identifier.* The identifier is not an
//!   attribute at all. A caller probing for capabilities reads this as "this
//!   driver has never heard of it".
//! * `HYC00` - *Optional feature not implemented.* The identifier is a real
//!   attribute this driver understands but does not act on. Callers may retry
//!   without it, or fall back to a connection-string keyword.
//!
//! Answering `HY092` for a recognized-but-unimplemented attribute is a parity
//! break: `mssql-python` forwards arbitrary identifiers through `attrs_before`
//! and `set_attr` with no filtering, so a caller probing
//! `SQL_COPT_SS_MARS_ENABLED` would conclude the identifier is bogus rather
//! than merely unavailable.
//!
//! The tables below record which identifiers msodbcsql recognizes. They are not
//! transcribed from the headers - a header defines far more constants than any
//! switch handles, and several of those constants are attribute *values* rather
//! than identifiers. Each identifier was instead probed against a live `ODBC
//! Driver 18 for SQL Server`, and counts as recognized when the probe returned
//! anything other than `HY092`. `docs/attributes_plan.md` §8 documents the
//! harness and how to reproduce the measurement.
//!
//! Two properties of the measured data drive the shape of this table:
//!
//! * **Lookup is scope-keyed**, because the vendor ranges overlap.
//!   `SQL_COPT_SS_*` and `SQL_SOPT_SS_*` share 1225-1238
//!   (`SQL_COPT_SS_FAILOVER_PARTNER` and `SQL_SOPT_SS_TEXTPTR_LOGGING` are both
//!   1225), and `SQL_ATTR_OUTPUT_NTS` (environment) collides with
//!   `SQL_ATTR_AUTO_IPD` (connection) at 10001. A flat identifier map would
//!   answer for the wrong scope.
//! * **Lookup is also operation-keyed**, because recognition is not symmetric.
//!   msodbcsql accepts the ODBC 2.x statement options on a *connection* handle
//!   and fans them out to every statement (`sqlcmisc.cpp:2879`,
//!   `IsSetStmtOptionValid`), yet `SQLGetConnectAttrW` rejects those same
//!   identifiers with `HY092`. `SQL_ATTR_QUERY_TIMEOUT` on a connection is
//!   settable but not readable.
//!
//! Environment attributes are absent from both tables. The Driver Manager
//! resolves `SQL_ATTR_ODBC_VERSION`, `SQL_ATTR_CONNECTION_POOLING` and
//! `SQL_ATTR_CP_MATCH` itself and never dispatches them, which the sweep
//! confirmed: all three answer `HY092` when aimed at a connection or statement.

use crate::api::odbc_types::SqlInteger;
use crate::api::sqlstate::{
    DiagMsg, ERR_INVALID_ATTRIBUTE_IDENTIFIER, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED,
};

/// Handle scope an attribute identifier is interpreted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrScope {
    /// Connection attributes (`SQLSetConnectAttrW` / `SQLGetConnectAttrW`).
    Dbc,
    /// Statement attributes (`SQLSetStmtAttrW` / `SQLGetStmtAttrW`).
    Stmt,
}

/// Which half of the attribute API an identifier is being used through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrOp {
    Set,
    Get,
}

const OP_SET: u8 = 1 << 0;
const OP_GET: u8 = 1 << 1;

/// `(identifier, msodbcsql name, operations msodbcsql recognizes it for)`,
/// sorted by identifier so lookup can binary search.
type AttrRow = (SqlInteger, &'static str, u8);

/// Connection-scope identifiers msodbcsql recognizes.
///
/// The 0-29 band is the ODBC 2.x statement-option fan-out: msodbcsql accepts
/// statement options on a connection handle and applies them to every statement
/// on that connection, but refuses to read them back. This driver implements
/// the fan-out only for `SQL_ATTR_QUERY_TIMEOUT`; the rest are listed so they
/// report `HYC00` rather than `HY092`.
static DBC_ATTRS: &[AttrRow] = &[
    (0, "SQL_ATTR_QUERY_TIMEOUT", OP_SET),
    (1, "SQL_ATTR_MAX_ROWS", OP_SET),
    (2, "SQL_ATTR_NOSCAN", OP_SET),
    (3, "SQL_ATTR_MAX_LENGTH", OP_SET),
    (4, "SQL_ATTR_ASYNC_ENABLE", OP_SET | OP_GET),
    (5, "SQL_ATTR_ROW_BIND_TYPE", OP_SET),
    (6, "SQL_ATTR_CURSOR_TYPE", OP_SET),
    (7, "SQL_ATTR_CONCURRENCY", OP_SET),
    (8, "SQL_ATTR_KEYSET_SIZE", OP_SET),
    (10, "SQL_ATTR_SIMULATE_CURSOR", OP_SET),
    (11, "SQL_ATTR_RETRIEVE_DATA", OP_SET),
    (12, "SQL_ATTR_USE_BOOKMARKS", OP_SET),
    (29, "SQL_ATTR_ASYNC_STMT_EVENT", OP_SET),
    (101, "SQL_ATTR_ACCESS_MODE", OP_SET | OP_GET),
    (102, "SQL_ATTR_AUTOCOMMIT", OP_SET | OP_GET),
    (103, "SQL_ATTR_LOGIN_TIMEOUT", OP_SET | OP_GET),
    (104, "SQL_ATTR_TRACE", OP_SET | OP_GET),
    (105, "SQL_ATTR_TRACEFILE", OP_SET | OP_GET),
    (106, "SQL_ATTR_TRANSLATE_LIB", OP_SET | OP_GET),
    (107, "SQL_ATTR_TRANSLATE_OPTION", OP_SET | OP_GET),
    (108, "SQL_ATTR_TXN_ISOLATION", OP_SET | OP_GET),
    (109, "SQL_ATTR_CURRENT_CATALOG", OP_SET | OP_GET),
    (110, "SQL_ATTR_ODBC_CURSORS", OP_SET | OP_GET),
    (112, "SQL_ATTR_PACKET_SIZE", OP_SET | OP_GET),
    (113, "SQL_ATTR_CONNECTION_TIMEOUT", OP_SET | OP_GET),
    (114, "SQL_ATTR_DISCONNECT_BEHAVIOR", OP_SET),
    (117, "SQL_ATTR_ASYNC_DBC_FUNCTIONS_ENABLE", OP_SET | OP_GET),
    (119, "SQL_ATTR_ASYNC_DBC_EVENT", OP_SET | OP_GET),
    (1202, "SQL_COPT_SS_USE_PROC_FOR_PREP", OP_SET | OP_GET),
    (1203, "SQL_COPT_SS_INTEGRATED_SECURITY", OP_SET | OP_GET),
    (1204, "SQL_COPT_SS_PRESERVE_CURSORS", OP_SET | OP_GET),
    (1205, "SQL_COPT_SS_USER_DATA", OP_SET | OP_GET),
    (1206, "SQL_COPT_SS_ANSI_OEM", OP_SET | OP_GET),
    (1207, "SQL_ATTR_ENLIST_IN_DTC", OP_SET),
    (1208, "SQL_ATTR_ENLIST_IN_XA", OP_SET),
    (1209, "SQL_ATTR_CONNECTION_DEAD", OP_GET),
    (1210, "SQL_COPT_SS_FALLBACK_CONNECT", OP_SET | OP_GET),
    (1211, "SQL_COPT_SS_PERF_DATA", OP_SET | OP_GET),
    (1212, "SQL_COPT_SS_PERF_DATA_LOG", OP_SET | OP_GET),
    (1213, "SQL_COPT_SS_PERF_QUERY_INTERVAL", OP_SET | OP_GET),
    (1214, "SQL_COPT_SS_PERF_QUERY_LOG", OP_SET | OP_GET),
    (1215, "SQL_COPT_SS_PERF_QUERY", OP_SET | OP_GET),
    (1216, "SQL_COPT_SS_PERF_DATA_LOG_NOW", OP_SET),
    (1217, "SQL_COPT_SS_QUOTED_IDENT", OP_SET | OP_GET),
    (1218, "SQL_COPT_SS_ANSI_NPW", OP_SET | OP_GET),
    (1219, "SQL_COPT_SS_BCP", OP_SET | OP_GET),
    (1220, "SQL_COPT_SS_TRANSLATE", OP_SET | OP_GET),
    (1221, "SQL_COPT_SS_ATTACHDBFILENAME", OP_SET | OP_GET),
    (1222, "SQL_COPT_SS_CONCAT_NULL", OP_SET | OP_GET),
    (1223, "SQL_COPT_SS_ENCRYPT", OP_SET | OP_GET),
    (1224, "SQL_COPT_SS_MARS_ENABLED", OP_SET | OP_GET),
    (1225, "SQL_COPT_SS_FAILOVER_PARTNER", OP_SET | OP_GET),
    (1226, "SQL_COPT_SS_OLDPWD", OP_SET),
    (1227, "SQL_COPT_SS_TXN_ISOLATION", OP_SET | OP_GET),
    (
        1228,
        "SQL_COPT_SS_TRUST_SERVER_CERTIFICATE",
        OP_SET | OP_GET,
    ),
    (1229, "SQL_COPT_SS_SERVER_SPN", OP_SET | OP_GET),
    (1230, "SQL_COPT_SS_FAILOVER_PARTNER_SPN", OP_SET | OP_GET),
    (1231, "SQL_COPT_SS_INTEGRATED_AUTHENTICATION_METHOD", OP_GET),
    (1232, "SQL_COPT_SS_MUTUALLY_AUTHENTICATED", OP_GET),
    (1233, "SQL_COPT_SS_CLIENT_CONNECTION_ID", OP_GET),
    (1234, "SQL_COPT_SS_CONNECT_RETRY_COUNT", OP_GET),
    (1235, "SQL_COPT_SS_CONNECT_RETRY_INTERVAL", OP_GET),
    (1236, "SQL_COPT_SS_CLIENT_CERTIFICATE", OP_SET | OP_GET),
    (
        1237,
        "SQL_COPT_SS_CLIENT_CERTIFICATE_FALLBACK",
        OP_SET | OP_GET,
    ),
    (1238, "SQL_COPT_SS_CLIENT_CONNECTION_ID_POINTER", OP_SET),
    (
        1239,
        "SQL_COPT_SS_CLIENT_CONNECTION_ID_REDIRECTED_POINTER",
        OP_SET,
    ),
    (
        1240,
        "SQL_COPT_SS_SERVER_CERTIFICATE_VALIDATION_CALLBACK",
        OP_SET,
    ),
    (1241, "SQL_COPT_SS_BROWSE_CONNECT", OP_SET | OP_GET),
    (1242, "SQL_COPT_SS_BROWSE_SERVER", OP_SET | OP_GET),
    (1243, "SQL_COPT_SS_WARN_ON_CP_ERROR", OP_SET | OP_GET),
    (1244, "SQL_COPT_SS_CONNECTION_DEAD", OP_GET),
    (1245, "SQL_COPT_SS_BROWSE_CACHE_DATA", OP_SET | OP_GET),
    (1246, "SQL_COPT_SS_RESET_CONNECTION", OP_SET),
    (1247, "SQL_COPT_SS_APPLICATION_INTENT", OP_SET | OP_GET),
    (1248, "SQL_COPT_SS_MULTISUBNET_FAILOVER", OP_SET | OP_GET),
    (1249, "SQL_COPT_SS_TNIR", OP_SET | OP_GET),
    (1250, "SQL_COPT_SS_COLUMN_ENCRYPTION", OP_SET | OP_GET),
    (1251, "SQL_COPT_SS_CEKEYSTOREPROVIDER", OP_SET | OP_GET),
    (1252, "SQL_COPT_SS_CEKEYSTOREDATA", OP_SET),
    (1253, "SQL_COPT_SS_TRUSTEDCMKPATHS", OP_SET | OP_GET),
    (1254, "SQL_COPT_SS_CEKCACHETTL", OP_SET | OP_GET),
    (1255, "SQL_COPT_SS_AUTHENTICATION", OP_SET | OP_GET),
    (1256, "SQL_COPT_SS_ACCESS_TOKEN", OP_SET | OP_GET),
    (
        1400,
        "SQL_COPT_SS_DATACLASSIFICATION_VERSION",
        OP_SET | OP_GET,
    ),
    (1401, "SQL_COPT_SS_SPID", OP_GET),
    (1402, "SQL_COPT_SS_AUTOBEGINTXN", OP_SET),
    (1403, "SQL_COPT_SS_LONGASMAX", OP_SET | OP_GET),
    (1404, "SQL_COPT_SS_GETDATA_EXTENSIONS", OP_SET | OP_GET),
    (10001, "SQL_ATTR_AUTO_IPD", OP_GET),
    (10014, "SQL_ATTR_METADATA_ID", OP_SET | OP_GET),
];

/// Statement-scope identifiers msodbcsql recognizes.
///
/// `SQL_ATTR_ENLIST_IN_DTC` (1207) is deliberately absent: it is a connection
/// attribute, and the sweep confirmed msodbcsql answers `HY092` for it on a
/// statement handle even though the identifier sits in the shared vendor range.
static STMT_ATTRS: &[AttrRow] = &[
    (0, "SQL_ATTR_QUERY_TIMEOUT", OP_SET | OP_GET),
    (1, "SQL_ATTR_MAX_ROWS", OP_SET | OP_GET),
    (2, "SQL_ATTR_NOSCAN", OP_SET | OP_GET),
    (3, "SQL_ATTR_MAX_LENGTH", OP_SET | OP_GET),
    (4, "SQL_ATTR_ASYNC_ENABLE", OP_SET | OP_GET),
    (5, "SQL_ATTR_ROW_BIND_TYPE", OP_SET | OP_GET),
    (6, "SQL_ATTR_CURSOR_TYPE", OP_SET | OP_GET),
    (7, "SQL_ATTR_CONCURRENCY", OP_SET | OP_GET),
    (8, "SQL_ATTR_KEYSET_SIZE", OP_SET | OP_GET),
    (10, "SQL_ATTR_SIMULATE_CURSOR", OP_SET | OP_GET),
    (11, "SQL_ATTR_RETRIEVE_DATA", OP_SET | OP_GET),
    (12, "SQL_ATTR_USE_BOOKMARKS", OP_SET | OP_GET),
    (14, "SQL_ATTR_ROW_NUMBER", OP_GET),
    (15, "SQL_ATTR_ENABLE_AUTO_IPD", OP_SET | OP_GET),
    (16, "SQL_ATTR_FETCH_BOOKMARK_PTR", OP_SET | OP_GET),
    (17, "SQL_ATTR_PARAM_BIND_OFFSET_PTR", OP_SET | OP_GET),
    (18, "SQL_ATTR_PARAM_BIND_TYPE", OP_SET | OP_GET),
    (19, "SQL_ATTR_PARAM_OPERATION_PTR", OP_SET | OP_GET),
    (20, "SQL_ATTR_PARAM_STATUS_PTR", OP_SET | OP_GET),
    (21, "SQL_ATTR_PARAMS_PROCESSED_PTR", OP_SET | OP_GET),
    (22, "SQL_ATTR_PARAMSET_SIZE", OP_SET | OP_GET),
    (23, "SQL_ATTR_ROW_BIND_OFFSET_PTR", OP_SET | OP_GET),
    (24, "SQL_ATTR_ROW_OPERATION_PTR", OP_SET | OP_GET),
    (25, "SQL_ATTR_ROW_STATUS_PTR", OP_SET | OP_GET),
    (26, "SQL_ATTR_ROWS_FETCHED_PTR", OP_SET | OP_GET),
    (27, "SQL_ATTR_ROW_ARRAY_SIZE", OP_SET | OP_GET),
    (29, "SQL_ATTR_ASYNC_STMT_EVENT", OP_SET),
    (1225, "SQL_SOPT_SS_TEXTPTR_LOGGING", OP_SET | OP_GET),
    (1226, "SQL_SOPT_SS_CURRENT_COMMAND", OP_GET),
    (1227, "SQL_SOPT_SS_HIDDEN_COLUMNS", OP_SET | OP_GET),
    (1228, "SQL_SOPT_SS_NOBROWSETABLE", OP_SET | OP_GET),
    (1229, "SQL_SOPT_SS_REGIONALIZE", OP_SET | OP_GET),
    (1230, "SQL_SOPT_SS_CURSOR_OPTIONS", OP_SET | OP_GET),
    (1231, "SQL_SOPT_SS_NOCOUNT_STATUS", OP_GET),
    (1232, "SQL_SOPT_SS_DEFER_PREPARE", OP_SET | OP_GET),
    (
        1233,
        "SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT",
        OP_SET | OP_GET,
    ),
    (
        1234,
        "SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT",
        OP_SET | OP_GET,
    ),
    (
        1235,
        "SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS",
        OP_SET | OP_GET,
    ),
    (1236, "SQL_SOPT_SS_PARAM_FOCUS", OP_SET | OP_GET),
    (1237, "SQL_SOPT_SS_NAME_SCOPE", OP_SET | OP_GET),
    (1238, "SQL_SOPT_SS_COLUMN_ENCRYPTION", OP_SET | OP_GET),
    (10010, "SQL_ATTR_APP_ROW_DESC", OP_SET | OP_GET),
    (10011, "SQL_ATTR_APP_PARAM_DESC", OP_SET | OP_GET),
    (10012, "SQL_ATTR_IMP_ROW_DESC", OP_SET | OP_GET),
    (10013, "SQL_ATTR_IMP_PARAM_DESC", OP_SET | OP_GET),
    (10014, "SQL_ATTR_METADATA_ID", OP_SET | OP_GET),
];

/// Returns the msodbcsql name for `attribute`, when the native driver
/// recognizes it for this exact scope and operation.
pub(crate) fn native_attr_name(
    scope: AttrScope,
    op: AttrOp,
    attribute: SqlInteger,
) -> Option<&'static str> {
    let table = match scope {
        AttrScope::Dbc => DBC_ATTRS,
        AttrScope::Stmt => STMT_ATTRS,
    };
    let wanted = match op {
        AttrOp::Set => OP_SET,
        AttrOp::Get => OP_GET,
    };
    let i = table
        .binary_search_by_key(&attribute, |&(id, _, _)| id)
        .ok()?;
    let (_, name, ops) = table[i];
    (ops & wanted != 0).then_some(name)
}

/// Picks the diagnostic for an attribute this driver does not implement.
///
/// `HYC00` when msodbcsql knows the identifier for this scope and operation,
/// `HY092` when neither driver does. The caller posts the diagnostic and
/// returns `SQL_ERROR`.
pub(crate) fn unimplemented_attr_diag(
    scope: AttrScope,
    op: AttrOp,
    attribute: SqlInteger,
) -> DiagMsg {
    match native_attr_name(scope, op, attribute) {
        Some(name) => {
            tracing::debug!(
                attribute,
                name,
                ?scope,
                ?op,
                "attribute recognized by msodbcsql but not implemented"
            );
            ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED
        }
        None => {
            tracing::error!(attribute, ?scope, ?op, "unrecognized attribute identifier");
            ERR_INVALID_ATTRIBUTE_IDENTIFIER
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sqlstate::{SQLSTATE_HY092, SQLSTATE_HYC00};

    const SCOPES: [AttrScope; 2] = [AttrScope::Dbc, AttrScope::Stmt];
    const OPS: [AttrOp; 2] = [AttrOp::Set, AttrOp::Get];

    fn table_of(scope: AttrScope) -> &'static [AttrRow] {
        match scope {
            AttrScope::Dbc => DBC_ATTRS,
            AttrScope::Stmt => STMT_ATTRS,
        }
    }

    /// `binary_search_by_key` returns arbitrary answers on an unsorted slice,
    /// so guard the invariant the lookup depends on. A regenerated table that
    /// came out unsorted fails here rather than silently mis-answering. An
    /// entry with no operation flags would be unreachable dead data.
    #[test]
    fn tables_are_sorted_unique_and_flagged() {
        for scope in SCOPES {
            let table = table_of(scope);
            for pair in table.windows(2) {
                assert!(
                    pair[0].0 < pair[1].0,
                    "{scope:?} table not strictly ascending at {} -> {}",
                    pair[0].0,
                    pair[1].0
                );
            }
            for &(id, name, ops) in table {
                assert!(ops & (OP_SET | OP_GET) != 0, "{name} ({id}) has no ops");
                assert!(ops & !(OP_SET | OP_GET) == 0, "{name} ({id}) has junk ops");
            }
        }
    }

    /// Every entry must resolve to its own name for at least one operation, and
    /// must not resolve for an operation it is not flagged for.
    #[test]
    fn every_table_entry_round_trips() {
        for scope in SCOPES {
            for &(id, name, ops) in table_of(scope) {
                for (op, bit) in [(AttrOp::Set, OP_SET), (AttrOp::Get, OP_GET)] {
                    let got = native_attr_name(scope, op, id);
                    if ops & bit != 0 {
                        assert_eq!(got, Some(name), "{name} ({id}) {op:?}");
                    } else {
                        assert_eq!(got, None, "{name} ({id}) {op:?}");
                    }
                }
            }
        }
    }

    /// The overlapping vendor ranges are the reason lookup is scope-keyed.
    #[test]
    fn vendor_ranges_resolve_per_scope() {
        assert_eq!(
            native_attr_name(AttrScope::Dbc, AttrOp::Set, 1225),
            Some("SQL_COPT_SS_FAILOVER_PARTNER")
        );
        assert_eq!(
            native_attr_name(AttrScope::Stmt, AttrOp::Set, 1225),
            Some("SQL_SOPT_SS_TEXTPTR_LOGGING")
        );
        assert_eq!(
            native_attr_name(AttrScope::Dbc, AttrOp::Get, 10001),
            Some("SQL_ATTR_AUTO_IPD")
        );
        assert_eq!(native_attr_name(AttrScope::Stmt, AttrOp::Get, 10001), None);
    }

    /// The ODBC 2.x statement options are settable on a connection but not
    /// readable back - the asymmetry that forced the per-operation flag.
    #[test]
    fn statement_options_on_a_connection_are_set_only() {
        for id in [0, 1, 2, 3, 5, 6, 7, 8, 10, 11, 12, 29] {
            assert!(
                native_attr_name(AttrScope::Dbc, AttrOp::Set, id).is_some(),
                "id {id} should be settable on a connection"
            );
            assert_eq!(
                native_attr_name(AttrScope::Dbc, AttrOp::Get, id),
                None,
                "id {id} should not be readable from a connection"
            );
        }
    }

    /// Connection attributes on a statement, and vice versa, are `HY092` in
    /// msodbcsql. Measured, not assumed.
    #[test]
    fn cross_scope_identifiers_are_unknown() {
        for op in OPS {
            // SQL_ATTR_CURRENT_CATALOG / SQL_ATTR_TXN_ISOLATION are dbc-only.
            assert!(native_attr_name(AttrScope::Stmt, op, 109).is_none());
            assert!(native_attr_name(AttrScope::Stmt, op, 108).is_none());
            // The descriptor handles and rowset controls are stmt-only.
            assert!(native_attr_name(AttrScope::Dbc, op, 10010).is_none());
            assert!(native_attr_name(AttrScope::Dbc, op, 27).is_none());
            // SQL_ATTR_ENLIST_IN_DTC is dbc-only despite the shared range.
            assert!(native_attr_name(AttrScope::Stmt, op, 1207).is_none());
        }
        assert!(native_attr_name(AttrScope::Dbc, AttrOp::Set, 1207).is_some());
    }

    /// Environment attributes never reach the driver, so neither table claims
    /// them: SQL_ATTR_ODBC_VERSION (200), SQL_ATTR_CONNECTION_POOLING (201),
    /// SQL_ATTR_CP_MATCH (202).
    #[test]
    fn environment_attributes_are_absent_from_both_tables() {
        for id in [200, 201, 202] {
            for scope in SCOPES {
                for op in OPS {
                    assert!(native_attr_name(scope, op, id).is_none(), "id {id}");
                }
            }
        }
    }

    #[test]
    fn known_identifier_maps_to_hyc00() {
        // SQL_COPT_SS_MARS_ENABLED: recognized by msodbcsql either way.
        assert_eq!(
            unimplemented_attr_diag(AttrScope::Dbc, AttrOp::Set, 1224).state,
            SQLSTATE_HYC00
        );
        assert_eq!(
            unimplemented_attr_diag(AttrScope::Dbc, AttrOp::Get, 1224).state,
            SQLSTATE_HYC00
        );
        // SQL_SOPT_SS_DEFER_PREPARE on a statement.
        assert_eq!(
            unimplemented_attr_diag(AttrScope::Stmt, AttrOp::Set, 1232).state,
            SQLSTATE_HYC00
        );
    }

    #[test]
    fn unknown_identifier_maps_to_hy092() {
        for scope in SCOPES {
            for op in OPS {
                assert_eq!(
                    unimplemented_attr_diag(scope, op, 99999).state,
                    SQLSTATE_HY092
                );
            }
        }
    }

    /// A set-only identifier must still report `HY092` on the get path, so the
    /// diagnostic follows the flag rather than mere table membership.
    #[test]
    fn set_only_identifier_maps_to_hy092_on_get() {
        assert_eq!(
            unimplemented_attr_diag(AttrScope::Dbc, AttrOp::Set, 1).state,
            SQLSTATE_HYC00
        );
        assert_eq!(
            unimplemented_attr_diag(AttrScope::Dbc, AttrOp::Get, 1).state,
            SQLSTATE_HY092
        );
    }

    /// `attrs_before` forwards arbitrary integers, so no identifier may panic
    /// and every one must classify. A full `i32` sweep is too slow for a unit
    /// test; the boundaries plus a stride over the populated region cover the
    /// interesting shape.
    #[test]
    fn every_identifier_classifies_without_panicking() {
        let probes = [SqlInteger::MIN, SqlInteger::MIN + 1, -1, 0]
            .into_iter()
            .chain((0..12_000).step_by(7))
            .chain([SqlInteger::MAX - 1, SqlInteger::MAX]);
        for id in probes {
            for scope in SCOPES {
                for op in OPS {
                    let expected = if native_attr_name(scope, op, id).is_some() {
                        SQLSTATE_HYC00
                    } else {
                        SQLSTATE_HY092
                    };
                    assert_eq!(
                        unimplemented_attr_diag(scope, op, id).state,
                        expected,
                        "id {id} {scope:?} {op:?}"
                    );
                }
            }
        }
    }

    /// Negative identifiers are reachable from Python, whose `attrs_before`
    /// keys are unbounded ints, and must never be treated as recognized.
    #[test]
    fn negative_identifiers_are_unknown() {
        for id in [-1, -1000, SqlInteger::MIN] {
            for scope in SCOPES {
                for op in OPS {
                    assert!(native_attr_name(scope, op, id).is_none(), "id {id}");
                }
            }
        }
    }
}
