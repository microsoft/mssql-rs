// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLSTATE constants and the SQL Server error-number → SQLSTATE map.

use crate::error::{HasDiagnostics, post_sql_error};
use mssql_tds::error::Error as TdsError;
use mssql_tds::error::SqlInfoMessage;

pub(crate) const SQLSTATE_01000: [u8; 5] = *b"01000";
pub(crate) const SQLSTATE_01004: [u8; 5] = *b"01004";
pub(crate) const SQLSTATE_01S00: [u8; 5] = *b"01S00";
pub(crate) const SQLSTATE_07009: [u8; 5] = *b"07009";
pub(crate) const SQLSTATE_08001: [u8; 5] = *b"08001";
pub(crate) const SQLSTATE_08003: [u8; 5] = *b"08003";
pub(crate) const SQLSTATE_24000: [u8; 5] = *b"24000";
pub(crate) const SQLSTATE_HY000: [u8; 5] = *b"HY000";
pub(crate) const SQLSTATE_HYC00: [u8; 5] = *b"HYC00";
pub(crate) const SQLSTATE_HY009: [u8; 5] = *b"HY009";
pub(crate) const SQLSTATE_HY010: [u8; 5] = *b"HY010";
pub(crate) const SQLSTATE_HY011: [u8; 5] = *b"HY011";
pub(crate) const SQLSTATE_HY024: [u8; 5] = *b"HY024";
pub(crate) const SQLSTATE_HY092: [u8; 5] = *b"HY092";
pub(crate) const SQLSTATE_HY110: [u8; 5] = *b"HY110";

// Driver-raised diagnostics: a fixed SQLSTATE paired with its canonical
// message text. Bundling the two means a call site posts one value and can't
// accidentally pair a message with the wrong SQLSTATE. This mirrors a
// message-resource table that binds a state to a string. Several
// entries can share a SQLSTATE (HY000 is the general-error state), so these are
// keyed by logical error, not by state. Server-originated errors don't use
// this — their text comes from the wire (see `post_tds_error`).

/// A driver-raised diagnostic: a fixed SQLSTATE and its canonical message.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiagMsg {
    pub(crate) state: [u8; 5],
    pub(crate) text: &'static str,
}

pub(crate) const ERR_INVALID_CURSOR_STATE: DiagMsg = DiagMsg {
    state: SQLSTATE_24000,
    text: "Invalid cursor state",
};
pub(crate) const ERR_CONNECTION_DOES_NOT_EXIST: DiagMsg = DiagMsg {
    state: SQLSTATE_08003,
    text: "Connection does not exist",
};
pub(crate) const ERR_NO_ACTIVE_TDS_CLIENT: DiagMsg = DiagMsg {
    state: SQLSTATE_HY000,
    text: "No active TDS client",
};
pub(crate) const ERR_CONNECTION_BUSY: DiagMsg = DiagMsg {
    state: SQLSTATE_HY000,
    text: "Connection is busy with results for another command",
};
pub(crate) const ERR_FUNCTION_SEQUENCE: DiagMsg = DiagMsg {
    state: SQLSTATE_HY010,
    text: "Function sequence error",
};
pub(crate) const ERR_INVALID_DESCRIPTOR_INDEX: DiagMsg = DiagMsg {
    state: SQLSTATE_07009,
    text: "Invalid descriptor index",
};
pub(crate) const ERR_STRING_RIGHT_TRUNCATION: DiagMsg = DiagMsg {
    state: SQLSTATE_01004,
    text: "String data, right truncation",
};
pub(crate) const ERR_INVALID_NULL_POINTER: DiagMsg = DiagMsg {
    state: SQLSTATE_HY009,
    text: "Invalid use of null pointer",
};
pub(crate) const ERR_INVALID_ATTRIBUTE_VALUE: DiagMsg = DiagMsg {
    state: SQLSTATE_HY024,
    text: "Invalid attribute value",
};
pub(crate) const ERR_INVALID_ATTRIBUTE_IDENTIFIER: DiagMsg = DiagMsg {
    state: SQLSTATE_HY092,
    text: "Invalid attribute/option identifier",
};
pub(crate) const ERR_INVALID_CONNECTION_STRING_ATTRIBUTE: DiagMsg = DiagMsg {
    state: SQLSTATE_01S00,
    text: "Invalid connection string attribute",
};

/// Post a driver-raised diagnostic (fixed SQLSTATE + canonical message) with
/// native error 0. For server-originated errors use [`post_tds_error`].
pub(crate) fn post_diag(state: &mut impl HasDiagnostics, msg: DiagMsg) {
    post_sql_error(state, msg.state, 0, msg.text);
}

/// SQL Server engine error number → ODBC 3.x SQLSTATE.
///
/// We keep only the 3.x state (not 2.x) since that is the behavior
/// modern apps (`SQL_OV_ODBC3` / `SQL_OV_ODBC3_80`) request.
///
/// Sorted by error number to allow binary search. Adding to this table is a
/// compatibility commitment: an entry must match the server's error semantics
/// exactly and the server team must agree the error number is frozen.
const SERVER_ERROR_TO_SQL_STATE_MAP: &[(u32, [u8; 5])] = &[
    (109, *b"21S01"),
    (110, *b"21S01"),
    (120, *b"07008"),
    (121, *b"07008"),
    (168, *b"22003"),
    (206, *b"22018"),
    (207, *b"42S22"),
    (208, *b"42S02"),
    (210, *b"22007"),
    (211, *b"22007"),
    (213, *b"21S01"),
    (220, *b"22003"),
    (229, *b"42000"),
    (230, *b"42000"),
    (232, *b"22003"),
    (233, *b"23000"),
    (234, *b"22003"),
    (235, *b"22018"),
    (236, *b"22003"),
    (237, *b"22003"),
    (238, *b"22003"),
    (241, *b"22007"),
    (242, *b"22007"),
    (244, *b"22003"),
    (245, *b"22018"),
    (246, *b"22003"),
    (248, *b"22003"),
    (256, *b"22018"),
    (266, *b"25000"),
    (267, *b"42S02"),
    (272, *b"23000"),
    (273, *b"23000"),
    (295, *b"22007"),
    (296, *b"22007"),
    // 305 was deprecated in sphinx/shiloh, reused in yukon.
    (305, *b"42000"),
    (310, *b"22025"),
    (409, *b"22018"),
    (512, *b"21000"),
    (515, *b"23000"),
    (517, *b"22007"),
    (518, *b"22018"),
    (529, *b"22018"),
    (535, *b"22003"),
    (544, *b"23000"),
    (547, *b"23000"),
    (550, *b"44000"),
    (628, *b"25000"),
    (911, *b"08004"),
    (916, *b"08004"),
    (919, *b"01000"),
    (1007, *b"22003"),
    (1010, *b"22019"),
    (1205, *b"40001"),
    (1211, *b"40001"),
    (1505, *b"23000"),
    (1508, *b"23000"),
    (1906, *b"42S02"),
    (1911, *b"42S22"),
    (1913, *b"42S11"),
    (2501, *b"42S02"),
    (2601, *b"23000"),
    (2615, *b"23000"),
    (2627, *b"23000"),
    (2705, *b"42S21"),
    (2706, *b"42S02"),
    (2714, *b"42S01"),
    (2727, *b"42S21"),
    (2740, *b"08004"),
    (3605, *b"23000"),
    (3606, *b"01000"),
    (3607, *b"01000"),
    (3622, *b"01000"),
    (3701, *b"42S02"),
    (3718, *b"42S12"),
    (3902, *b"25000"),
    (3903, *b"25000"),
    (3906, *b"25000"),
    (3908, *b"25000"),
    (4002, *b"28000"),
    (4017, *b"08004"),
    (4019, *b"08004"),
    (4401, *b"42S02"),
    (4409, *b"21S02"),
    (4501, *b"21S02"),
    (4502, *b"21S02"),
    (4506, *b"42S21"),
    (4701, *b"42S02"),
    (4902, *b"42S02"),
    (4924, *b"42S22"),
    (5701, *b"01000"),
    (5703, *b"01000"),
    (6401, *b"25000"),
    (7112, *b"40001"),
    (8101, *b"23000"),
    (8115, *b"22003"),
    (8134, *b"22012"),
    (8152, *b"22001"),
    (8153, *b"01003"),
    (16902, *b"HY109"),
    (16916, *b"34000"),
    (16930, *b"24000"),
    (16931, *b"24000"),
    (16934, *b"01001"),
    (16947, *b"01001"),
    (17809, *b"08004"),
    (18450, *b"08004"),
    (18452, *b"28000"),
    (18456, *b"28000"), // LOGON_FAILED — "Login failed for user"
    (18458, *b"08004"),
    (18459, *b"28000"),
    (18463, *b"28000"), // PASSWORD_CANTBEUSED
    (18464, *b"28000"), // PASSWORD_TOOSHORT
    (18465, *b"28000"), // PASSWORD_TOOLONG
    (18466, *b"28000"), // PASSWORD_TOOSIMPLE
    (18467, *b"28000"), // PASSWORD_FAILEDFILTER
    (18468, *b"28000"), // PASSWORD_CHANGEERROR
    (18487, *b"28000"), // PASSWORD_EXPIRED
    (18488, *b"28000"), // PASSWORD_MUSTCHANGE
];

/// Look up the ODBC 3.x SQLSTATE for a SQL Server engine error number.
///
/// Returns `None` if the error number is not in the table
pub(crate) fn sqlstate_for_sql_error(error_number: u32) -> Option<[u8; 5]> {
    SERVER_ERROR_TO_SQL_STATE_MAP
        .binary_search_by_key(&error_number, |&(n, _)| n)
        .ok()
        .map(|i| SERVER_ERROR_TO_SQL_STATE_MAP[i].1)
}

/// Post one ODBC diagnostic record per server error in `err`.
///
/// For [`TdsError::SqlServerError`], iterates the server-reported errors in
/// the order TDS delivered them and pushes one
/// [`DiagRecord`](crate::error::DiagRecord) each. Each record's SQLSTATE
/// comes from [`sqlstate_for_sql_error`]; any error number not in the map
/// falls back to `default`. Native error and message are taken straight
/// from the server-reported error. Any informational/warning messages the
/// server sent alongside the errors are posted after the error records so a
/// failing call still surfaces the full server diagnostic set (matching
/// msodbcsql).
///
/// For any non-`SqlServerError` variant (transport, TLS, redirect, protocol,
/// timeout, …), pushes a single record with `default`, native error 0, and
/// the error's `Display` text.
///
/// `default` is the SQLSTATE that best describes the caller's context —
/// typically `08001` for connect-time failures and `HY000` for general
/// execution / fetch failures.
pub(crate) fn post_tds_error(state: &mut impl HasDiagnostics, err: &TdsError, default: [u8; 5]) {
    if let TdsError::SqlServerError { diagnostics } = err {
        if diagnostics.errors.is_empty() {
            // An error return must surface at least one error record. If the
            // server diagnostics unexpectedly carried no ERROR tokens, post a
            // default record from the error's Display text so the caller still
            // sees a primary failure.
            post_sql_error(state, default, 0, err.to_string());
        } else {
            for e in &diagnostics.errors {
                let sqlstate = sqlstate_for_sql_error(e.number).unwrap_or(default);
                let native = i32::try_from(e.number).unwrap_or(i32::MAX);
                post_sql_error(state, sqlstate, native, e.message.clone());
            }
        }
        // Informational/warning records follow the primary error record(s).
        post_tds_info_messages(state, &diagnostics.info_messages);
        return;
    }
    post_sql_error(state, default, 0, err.to_string());
}

/// Post one ODBC diagnostic record per SQL Server INFO token.
///
/// Returns `true` when at least one record was posted. The caller can use that
/// to return `SQL_SUCCESS_WITH_INFO` for successful APIs that observed server
/// messages.
pub(crate) fn post_tds_info_messages(
    state: &mut impl HasDiagnostics,
    messages: &[SqlInfoMessage],
) -> bool {
    for message in messages {
        let sqlstate = sqlstate_for_sql_error(message.number).unwrap_or(SQLSTATE_01000);
        let native = i32::try_from(message.number).unwrap_or(i32::MAX);
        post_sql_error(state, sqlstate, native, message.message.clone());
    }

    !messages.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_is_sorted_and_unique() {
        for w in SERVER_ERROR_TO_SQL_STATE_MAP.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "SQL_STATE_MAP must be sorted with unique keys: {} >= {}",
                w[0].0,
                w[1].0
            );
        }
    }

    #[test]
    fn login_failed_maps_to_28000() {
        assert_eq!(sqlstate_for_sql_error(18456), Some(*b"28000"));
    }

    #[test]
    fn untrusted_domain_login_maps_to_28000() {
        assert_eq!(sqlstate_for_sql_error(18452), Some(*b"28000"));
    }

    #[test]
    fn db_open_failure_maps_to_08004() {
        // 18450 — login valid but DB access failed.
        assert_eq!(sqlstate_for_sql_error(18450), Some(*b"08004"));
    }

    #[test]
    fn password_must_change_maps_to_28000() {
        assert_eq!(sqlstate_for_sql_error(18488), Some(*b"28000"));
    }

    #[test]
    fn invalid_object_maps_to_42s02() {
        // 208 — Invalid object name.
        assert_eq!(sqlstate_for_sql_error(208), Some(*b"42S02"));
    }

    #[test]
    fn unknown_error_returns_none() {
        assert_eq!(sqlstate_for_sql_error(0), None);
        assert_eq!(sqlstate_for_sql_error(9999), None);
        assert_eq!(sqlstate_for_sql_error(u32::MAX), None);
    }

    use mssql_tds::error::{SqlErrorInfo, SqlInfoMessage};

    fn sql_error(number: u32, message: &str) -> SqlErrorInfo {
        SqlErrorInfo {
            message: message.into(),
            state: 1,
            class: 14,
            number,
            server_name: None,
            proc_name: None,
            line_number: None,
        }
    }

    #[derive(Default)]
    struct FakeState {
        records: Vec<crate::error::DiagRecord>,
    }
    impl HasDiagnostics for FakeState {
        fn diag_records(&self) -> &[crate::error::DiagRecord] {
            &self.records
        }
        fn diag_records_mut(&mut self) -> &mut Vec<crate::error::DiagRecord> {
            &mut self.records
        }
    }

    #[test]
    fn post_tds_error_single_server_error_posts_one_record() {
        let mut s = FakeState::default();
        let err = TdsError::from_sql_errors(vec![sql_error(18456, "Login failed for user 'x'.")]);
        post_tds_error(&mut s, &err, SQLSTATE_08001);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, *b"28000");
        assert_eq!(s.records[0].native_error, 18456);
        assert_eq!(s.records[0].message, "Login failed for user 'x'.");
    }

    #[test]
    fn post_tds_error_posts_one_record_per_server_error_in_order() {
        // 18456 → 28000 (mapped); 4060 → fallback (not in our map).
        let mut s = FakeState::default();
        let err = TdsError::from_sql_errors(vec![
            sql_error(18456, "Login failed."),
            sql_error(4060, "Cannot open database 'foo'."),
        ]);
        post_tds_error(&mut s, &err, SQLSTATE_08001);
        assert_eq!(s.records.len(), 2);
        assert_eq!(s.records[0].sql_state, *b"28000");
        assert_eq!(s.records[0].native_error, 18456);
        assert_eq!(s.records[1].sql_state, SQLSTATE_08001); // fallback
        assert_eq!(s.records[1].native_error, 4060);
    }

    #[test]
    fn post_tds_error_fans_out_errors_then_info_messages() {
        // A failing operation that also carried an INFO/warning message must
        // surface both: errors first (record 1 = primary failure), then info.
        use mssql_tds::error::SqlServerDiagnostics;
        let mut s = FakeState::default();
        let diagnostics = SqlServerDiagnostics::new(
            vec![sql_error(18456, "Login failed.")],
            vec![SqlInfoMessage {
                message: "Changed database context to 'master'.".into(),
                state: 1,
                class: 10,
                number: 5701,
                server_name: Some("srv".into()),
                proc_name: None,
                line_number: Some(1),
            }],
        );
        let err = TdsError::from_sql_diagnostics(diagnostics);
        post_tds_error(&mut s, &err, SQLSTATE_08001);
        assert_eq!(s.records.len(), 2);
        assert_eq!(s.records[0].sql_state, *b"28000");
        assert_eq!(s.records[0].native_error, 18456);
        assert_eq!(s.records[1].sql_state, *b"01000");
        assert_eq!(s.records[1].native_error, 5701);
    }

    #[test]
    fn post_tds_error_info_only_diagnostics_still_posts_primary_error() {
        // A SqlServerError that unexpectedly carries only INFO messages (no
        // ERROR tokens) must still surface a primary error record for the error
        // return, followed by the info record(s).
        use mssql_tds::error::SqlServerDiagnostics;
        let mut s = FakeState::default();
        let diagnostics = SqlServerDiagnostics::new(
            vec![],
            vec![SqlInfoMessage {
                message: "info only".into(),
                state: 1,
                class: 10,
                number: 5701,
                server_name: None,
                proc_name: None,
                line_number: Some(1),
            }],
        );
        let err = TdsError::from_sql_diagnostics(diagnostics);
        post_tds_error(&mut s, &err, SQLSTATE_HY000);
        assert_eq!(s.records.len(), 2);
        // Record 1 is the synthesized primary error (default state, native 0).
        assert_eq!(s.records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(s.records[0].native_error, 0);
        // Record 2 is the info message.
        assert_eq!(s.records[1].sql_state, *b"01000");
        assert_eq!(s.records[1].native_error, 5701);
    }

    #[test]
    fn post_tds_error_non_server_error_posts_single_default_record() {
        let mut s = FakeState::default();
        let err = TdsError::ProtocolError("bad packet".into());
        post_tds_error(&mut s, &err, SQLSTATE_HY000);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(s.records[0].native_error, 0);
    }

    #[test]
    fn post_tds_error_empty_server_error_vec_falls_back() {
        let mut s = FakeState::default();
        let err = TdsError::from_sql_errors(vec![]);
        post_tds_error(&mut s, &err, SQLSTATE_HY000);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(s.records[0].native_error, 0);
    }

    #[test]
    fn post_tds_info_messages_posts_records_and_reports_presence() {
        let mut s = FakeState::default();
        let messages = vec![
            SqlInfoMessage {
                message: "Changed database context to 'master'.".into(),
                state: 1,
                class: 10,
                number: 5701,
                server_name: Some("server".into()),
                proc_name: None,
                line_number: Some(1),
            },
            SqlInfoMessage {
                message: "hello from PRINT".into(),
                state: 1,
                class: 0,
                number: 0,
                server_name: Some("server".into()),
                proc_name: None,
                line_number: Some(1),
            },
        ];

        assert!(post_tds_info_messages(&mut s, &messages));
        assert_eq!(s.records.len(), 2);
        assert_eq!(s.records[0].sql_state, *b"01000");
        assert_eq!(s.records[0].native_error, 5701);
        assert_eq!(
            s.records[0].message,
            "Changed database context to 'master'."
        );
        assert_eq!(s.records[1].sql_state, SQLSTATE_01000);
        assert_eq!(s.records[1].native_error, 0);
        assert_eq!(s.records[1].message, "hello from PRINT");
    }

    #[test]
    fn post_tds_info_messages_empty_returns_false() {
        let mut s = FakeState::default();
        assert!(!post_tds_info_messages(&mut s, &[]));
        assert!(s.records.is_empty());
    }

    #[test]
    fn post_diag_uses_bundled_state_and_message() {
        let mut s = FakeState::default();
        post_diag(&mut s, ERR_CONNECTION_DOES_NOT_EXIST);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, SQLSTATE_08003);
        assert_eq!(s.records[0].native_error, 0);
        assert_eq!(s.records[0].message, "Connection does not exist");
    }
}
