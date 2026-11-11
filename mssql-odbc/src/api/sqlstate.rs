// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLSTATE constants and the SQL Server error-number → SQLSTATE map.

use crate::api::odbc_types::{SQL_NO_ROW_NUMBER, SqlLen};
use crate::error::diag::post_prefixed;
use crate::error::{DiagRecord, HasDiagnostics, post_sql_error};
use crate::handles::wire::ClaimError;
use mssql_tds::error::Error as TdsError;
use mssql_tds::error::{SqlErrorInfo, SqlInfoMessage};

pub(crate) const SQLSTATE_01000: [u8; 5] = *b"01000";
pub(crate) const SQLSTATE_01004: [u8; 5] = *b"01004";
pub(crate) const SQLSTATE_01S00: [u8; 5] = *b"01S00";
pub(crate) const SQLSTATE_01S02: [u8; 5] = *b"01S02";
pub(crate) const SQLSTATE_01S05: [u8; 5] = *b"01S05";
pub(crate) const SQLSTATE_01S07: [u8; 5] = *b"01S07";
pub(crate) const SQLSTATE_07002: [u8; 5] = *b"07002";
pub(crate) const SQLSTATE_07005: [u8; 5] = *b"07005";
pub(crate) const SQLSTATE_07006: [u8; 5] = *b"07006";
pub(crate) const SQLSTATE_07009: [u8; 5] = *b"07009";
pub(crate) const SQLSTATE_07S01: [u8; 5] = *b"07S01";
pub(crate) const SQLSTATE_08001: [u8; 5] = *b"08001";
pub(crate) const SQLSTATE_08003: [u8; 5] = *b"08003";
/// Connection failure during a transaction — ODBC's precise state for a
/// commit or rollback that could not reach the server.
pub(crate) const SQLSTATE_08007: [u8; 5] = *b"08007";
/// Communication link failure — the connection to the server broke, so a
/// connection pool must discard the connection rather than reuse it.
pub(crate) const SQLSTATE_08S01: [u8; 5] = *b"08S01";
pub(crate) const SQLSTATE_22001: [u8; 5] = *b"22001";
pub(crate) const SQLSTATE_22002: [u8; 5] = *b"22002";
pub(crate) const SQLSTATE_22003: [u8; 5] = *b"22003";
pub(crate) const SQLSTATE_22007: [u8; 5] = *b"22007";
pub(crate) const SQLSTATE_22008: [u8; 5] = *b"22008";
pub(crate) const SQLSTATE_22018: [u8; 5] = *b"22018";
pub(crate) const SQLSTATE_22026: [u8; 5] = *b"22026";
pub(crate) const SQLSTATE_24000: [u8; 5] = *b"24000";
pub(crate) const SQLSTATE_25000: [u8; 5] = *b"25000";
/// Syntax error or access violation — the state a server error carries when it
/// is the application's statement that is at fault.
pub(crate) const SQLSTATE_42000: [u8; 5] = *b"42000";
pub(crate) const SQLSTATE_HY000: [u8; 5] = *b"HY000";
pub(crate) const SQLSTATE_HY001: [u8; 5] = *b"HY001";
pub(crate) const SQLSTATE_HY003: [u8; 5] = *b"HY003";
pub(crate) const SQLSTATE_HY004: [u8; 5] = *b"HY004";
pub(crate) const SQLSTATE_HY012: [u8; 5] = *b"HY012";
pub(crate) const SQLSTATE_HYC00: [u8; 5] = *b"HYC00";
pub(crate) const SQLSTATE_HY009: [u8; 5] = *b"HY009";
pub(crate) const SQLSTATE_HY010: [u8; 5] = *b"HY010";
pub(crate) const SQLSTATE_HY011: [u8; 5] = *b"HY011";
pub(crate) const SQLSTATE_HY016: [u8; 5] = *b"HY016";
pub(crate) const SQLSTATE_HY017: [u8; 5] = *b"HY017";
pub(crate) const SQLSTATE_HY020: [u8; 5] = *b"HY020";
pub(crate) const SQLSTATE_HY021: [u8; 5] = *b"HY021";
pub(crate) const SQLSTATE_HY024: [u8; 5] = *b"HY024";
pub(crate) const SQLSTATE_HY094: [u8; 5] = *b"HY094";
pub(crate) const SQLSTATE_HY090: [u8; 5] = *b"HY090";
pub(crate) const SQLSTATE_HY091: [u8; 5] = *b"HY091";
pub(crate) const SQLSTATE_HY092: [u8; 5] = *b"HY092";
pub(crate) const SQLSTATE_HY106: [u8; 5] = *b"HY106";
pub(crate) const SQLSTATE_HY096: [u8; 5] = *b"HY096";
pub(crate) const SQLSTATE_HY104: [u8; 5] = *b"HY104";
pub(crate) const SQLSTATE_HY105: [u8; 5] = *b"HY105";
pub(crate) const SQLSTATE_HY110: [u8; 5] = *b"HY110";
// msodbcsql spells this IDS_S1_113; its `S1` prefix is the ODBC 2.x form of
// `HY` (IDS_S1_C00 is HYC00), so the 3.x state is HY113.
pub(crate) const SQLSTATE_HY113: [u8; 5] = *b"HY113";
/// Timeout expired — `SQL_ATTR_QUERY_TIMEOUT` (or `SQL_ATTR_CONNECTION_TIMEOUT`)
/// elapsed before the driver got a response. Distinct from `HY000` so an
/// application can tell "my deadline passed" from "something else broke".
pub(crate) const SQLSTATE_HYT00: [u8; 5] = *b"HYT00";
pub(crate) const SQLSTATE_HY008: [u8; 5] = *b"HY008";

// Driver-raised diagnostics: a fixed SQLSTATE paired with its canonical
// message text. Bundling the two means a call site posts one value and can't
// accidentally pair a message with the wrong SQLSTATE. This mirrors a
// message-resource table that binds a state to a string. Several
// entries can share a SQLSTATE (HY000 is the general-error state), so these are
// keyed by logical error, not by state. Server-originated errors don't use
// this — their text comes from the wire (see `post_tds_error`).

/// A driver-raised diagnostic: a fixed SQLSTATE and its canonical message.
///
/// Only `diag_msgs!` builds these, so `full` is always
/// [`DRIVER_DIAG_PREFIX`](crate::error::diag::DRIVER_DIAG_PREFIX) + `text`
/// resolved at compile time and [`post_diag`] can post it without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiagMsg {
    pub(crate) state: [u8; 5],
    pub(crate) text: &'static str,
    full: &'static str,
}

macro_rules! diag_msgs {
    ($($(#[$meta:meta])* $name:ident = ($state:expr, $text:literal $(,)?);)*) => {
        $(
            $(#[$meta])*
            pub(crate) const $name: DiagMsg = DiagMsg {
                state: $state,
                text: $text,
                full: concat!(crate::error::diag::driver_diag_prefix!(), $text),
            };
        )*

        #[cfg(test)]
        const ALL_DIAG_MSGS: &[(&str, DiagMsg)] = &[$((stringify!($name), $name)),*];
    };
}

impl From<crate::conversion::error::ConvError> for DiagMsg {
    fn from(error: crate::conversion::error::ConvError) -> Self {
        use crate::conversion::error::ConvError;

        match error {
            ConvError::OutOfRange => ERR_NUMERIC_OUT_OF_RANGE,
            ConvError::DatetimeFieldOverflow => ERR_DATETIME_FIELD_OVERFLOW,
            ConvError::InvalidDatetimeFormat => ERR_INVALID_DATETIME_FORMAT,
            ConvError::Restricted => ERR_RESTRICTED_DATA_TYPE,
            ConvError::InvalidCharacterValue => ERR_INVALID_CHARACTER_VALUE,
            ConvError::Internal => ERR_INTERNAL_CONVERSION,
            ConvError::NotHandledHere => ERR_COLUMN_CONVERSION_NOT_IMPLEMENTED,
        }
    }
}

diag_msgs! {
    ERR_COLUMN_CONVERSION_NOT_IMPLEMENTED = (
        SQLSTATE_HYC00,
        "Column type conversion not yet implemented"
    );
    ERR_INVALID_CURSOR_STATE = (SQLSTATE_24000, "Invalid cursor state");
    ERR_OPERATION_CANCELED = (SQLSTATE_HY008, "Operation canceled");
    ERR_CONNECTION_DOES_NOT_EXIST = (SQLSTATE_08003, "Connection does not exist");
    ERR_NO_ACTIVE_TDS_CLIENT = (SQLSTATE_HY000, "No active TDS client");
    ERR_CONNECTION_BUSY = (SQLSTATE_HY000, "Connection is busy with results for another command");
    ERR_STATEMENT_UNUSABLE = (
        SQLSTATE_HY000,
        "A statement on this connection is unusable and did not receive the attribute"
    );
    ERR_FUNCTION_SEQUENCE = (SQLSTATE_HY010, "Function sequence error");
    /// msodbcsql's text for describing a statement positioned on no result set
    /// (measured on 18.6.2.1).
    ERR_NOT_CURSOR_SPECIFICATION = (
        SQLSTATE_07005,
        "Prepared statement is not a cursor-specification"
    );
    /// No supported `SQL_ATTR_ODBC_VERSION` was recorded before the application
    /// allocated a connection. Reached either by a caller that skipped the
    /// attribute entirely, or by a 2.x application whose `SQL_OV_ODBC2` this
    /// driver rejected (`HY024`) — the Driver Manager forwards that declaration
    /// rather than mapping it, then allocates the connection regardless. ODBC
    /// specifies `HY010` for allocating a connection before the version is set.
    ///
    /// Note the text below reaches only a caller that loads this driver directly.
    /// It is posted on *this driver's* environment handle, which a Driver Manager
    /// application never holds — the application's `henv` is the DM's own. On the
    /// path this exists for, the DM substitutes its own diagnostic: unixODBC posts
    /// `IM005` ("Driver's SQLAllocHandle on SQL_HANDLE_DBC failed",
    /// `DriverManager/SQLConnect.c:1613-1616`), while the Windows DM propagates
    /// the `HY024` from the rejected `SQLSetEnvAttr` instead. `post_diag` only
    /// appends to `diag_records` and does not trace, so the refusal site in
    /// `alloc_handle.rs` logs this `text` verbatim — that log is the only place
    /// the reason survives for a Driver Manager user on either platform.
    ERR_ODBC_VERSION_NOT_SET = (
        SQLSTATE_HY010,
        "SQL_ATTR_ODBC_VERSION must be set to SQL_OV_ODBC3 or SQL_OV_ODBC3_80 \
         before allocating a connection; this driver does not support ODBC 2.x"
    );
    /// A fallible allocation failed. Used where the byte count comes from the
    /// application rather than a bounded internal computation -- e.g. buffering
    /// a data-at-execution value with no declared total (`SQL_DATA_AT_EXEC`) --
    /// so it fails as a diagnostic the caller can act on instead of hitting
    /// `Vec`'s default infallible allocation, which aborts the process this
    /// driver is loaded into.
    ERR_MEMORY_ALLOCATION = (SQLSTATE_HY001, "Memory allocation error");
    ERR_PLP_TYPED_LIMIT = (
        SQLSTATE_HYC00,
        "PLP typed conversion exceeds the materialization limit"
    );
    ERR_FETCH_TYPE_OUT_OF_RANGE = (SQLSTATE_HY106, "Fetch type out of range");
    ERR_INVALID_DESCRIPTOR_INDEX = (SQLSTATE_07009, "Invalid descriptor index");
    ERR_INVALID_DESCRIPTOR_FIELD = (SQLSTATE_HY091, "Invalid descriptor field identifier");
    /// Writing a record field (or any header field besides
    /// `SQL_DESC_ROWS_PROCESSED_PTR` / `SQL_DESC_ARRAY_STATUS_PTR`) on an
    /// implementation row descriptor. Matches msodbcsql's `IDS_HY_016`
    /// (`sqlcdesc.cpp:1399-1405`) — IRD is populated by the driver, never by the
    /// application.
    ERR_CANNOT_MODIFY_IRD = (SQLSTATE_HY016, "Cannot modify an implementation row descriptor");
    /// `SQLFreeHandle(SQL_HANDLE_DESC, ...)` on an implicitly-allocated
    /// descriptor, or `SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC/APP_PARAM_DESC, ...)`
    /// given an implicitly-allocated descriptor other than the statement's own
    /// ARD/APD (i.e. another statement's ARD/APD, or this statement's own
    /// IRD/IPD). Matches the ODBC reference's HY017 entries for both functions —
    /// implicit descriptors are owned by their statement and can never be freed
    /// or reassigned as if they were explicit handles.
    ERR_INVALID_USE_OF_AUTO_DESC = (
        SQLSTATE_HY017,
        "Invalid use of an automatically allocated descriptor handle"
    );
    /// `SQL_DESC_TYPE` set to the verbose `SQL_DATETIME` marker before a valid
    /// `SQL_DESC_DATETIME_INTERVAL_CODE` identifies which date/time member is
    /// meant. Matches msodbcsql's `IDS_HY_021` (`sqlcdesc.cpp:1676`).
    ERR_INCONSISTENT_DESCRIPTOR_INFO = (SQLSTATE_HY021, "Inconsistent descriptor information");
    /// `SQL_C_NUMERIC` precision/scale outside the valid range (precision
    /// `1..=SQL_MAX_NUMERIC_LEN` digits, scale `<= precision`). Matches
    /// msodbcsql's `IDS_S1_094` (`sqlcdesc.cpp:11384-11394`).
    ERR_INVALID_PRECISION_OR_SCALE = (SQLSTATE_HY094, "Invalid precision or scale value");
    /// `SQL_DESC_PARAMETER_TYPE` set to a value other than `SQL_PARAM_INPUT`,
    /// `SQL_PARAM_INPUT_OUTPUT`, or `SQL_PARAM_OUTPUT`.
    ERR_INVALID_PARAMETER_TYPE = (SQLSTATE_HY105, "Invalid parameter type");
    ERR_NOT_VARIANT_COLUMN = (
        SQLSTATE_HY113,
        "Variant operation requested on column that is not sql_variant"
    );
    ERR_UNBOUND_PARAMETER = (SQLSTATE_07002, "COUNT field incorrect or syntax error");
    ERR_INVALID_SQL_DATA_TYPE = (SQLSTATE_HY004, "Invalid SQL data type");
    ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED = (SQLSTATE_HYC00, "Optional feature not implemented");
    ERR_TNIR_NOT_SUPPORTED = (
        SQLSTATE_HYC00,
        "TransparentNetworkIPResolution is not supported; use MultiSubnetFailover instead"
    );
    ERR_INVALID_C_DATA_TYPE = (SQLSTATE_HY003, "Invalid application buffer type");
    ERR_RESTRICTED_DATA_TYPE = (SQLSTATE_07006, "Restricted data type attribute violation");
    ERR_INTERNAL_CONVERSION = (SQLSTATE_HY000, "Internal error converting value");
    // `SQL_DEFAULT_PARAM` is only legal for a canonical procedure call, which this
    // driver does not support, so the state is terminal rather than "not yet"
    // (msodbcsql `sqlccmd.cpp` -> IDS_07_S01 on a non-canonical call statement).
    ERR_INVALID_USE_OF_DEFAULT_PARAM = (SQLSTATE_07S01, "Invalid use of default parameter");
    ERR_DATA_AT_EXEC_NOT_STAGED = (
        SQLSTATE_HY000,
        "Data-at-execution parameter reached the materializing conversion path"
    );
    ERR_PARAM_C_TYPE_NOT_IMPLEMENTED = (SQLSTATE_HYC00, "Parameter C type not yet implemented");
    ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED = (SQLSTATE_HYC00, "Parameter SQL type not yet implemented");
    ERR_PARAM_CONVERSION_NOT_IMPLEMENTED = (
        SQLSTATE_HYC00,
        "Parameter conversion not yet implemented"
    );
    // Covers both `ColumnSize` and `DecimalDigits`, as the ODBC 3.x `HY104` entry
    // does. msodbcsql splits the text (IDS_S1_104 for precision, IDS_S1_094 for
    // scale) but reports the one state either way
    // (`Sql/Ntdbms/sqlncli/cli_common/src/clntcomn.cpp`). S1094 is ODBC 2.x and is not used here.
    ERR_INVALID_PARAM_PRECISION_OR_SCALE = (SQLSTATE_HY104, "Invalid precision or scale value");
    /// A bound column delivered NULL but the application supplied no indicator to
    /// receive it, so the value cannot be reported at all.
    ERR_INDICATOR_REQUIRED = (SQLSTATE_22002, "Indicator variable required but not supplied");
    ERR_NUMERIC_OUT_OF_RANGE = (SQLSTATE_22003, "Numeric value out of range");
    ERR_INVALID_CHARACTER_VALUE = (
        SQLSTATE_22018,
        "Invalid character value for cast specification"
    );
    /// Date/time fields that name no real instant - month 13, 31 February, a
    /// year outside `0001`..`9999`, or an out-of-range time or UTC offset.
    ERR_INVALID_DATETIME_FORMAT = (SQLSTATE_22007, "Invalid datetime format");
    /// Temporal arithmetic overflow or a fraction dropped by the target's declared scale.
    /// Retail 18.6.2.1
    /// answers this state for `time`, `datetime2` and `datetimeoffset` alike.
    ///
    /// `ParamToSQLType` (`sqlcfunc.cpp:3350`) reads as a split - this state for the
    /// timestamp family, `22001` otherwise - but that gate is the legacy datetime
    /// arm and is not what these targets reach; see `convert_datetime_sql`. Do not
    /// re-derive the split from source.
    ERR_DATETIME_FIELD_OVERFLOW = (SQLSTATE_22008, "Datetime field overflow");
    ERR_DAE_LENGTH_MISMATCH = (SQLSTATE_22026, "String data, length mismatch");
    /// A `SQL_SS_UDT` parameter bound without `SQL_CA_SS_UDT_TYPE_NAME`. msodbcsql
    /// rejects the same binding before execute, also as `HY000`
    /// (`sqlccmd.cpp:9977`, `IDS_S1_000_95`). The SQLSTATE is the parity claim;
    /// this text is this driver's own and is not derived from the reference.
    ERR_MISSING_UDT_TYPE_NAME = (
        SQLSTATE_HY000,
        "A UDT parameter requires SQL_CA_SS_UDT_TYPE_NAME to be set"
    );
    ERR_MISSING_TVP_TYPE_NAME = (
        SQLSTATE_HY000,
        "A table-valued parameter requires SQL_CA_SS_TYPE_NAME to be set"
    );
    // A streamed parameter is either NULL or a sequence of value chunks; mixing the
    // two in one `SQL_NEED_DATA` window has no representation on the wire
    // (msodbcsql `sqlccmd.cpp` -> IDS_HY_020).
    ERR_ATTEMPT_TO_CONCATENATE_NULL = (SQLSTATE_HY020, "Attempt to concatenate a null value");
    WARN_FRACTIONAL_TRUNCATION = (SQLSTATE_01S07, "Fractional truncation");
    /// Inbound truncation of a bound parameter. Distinct from
    /// [`WARN_STRING_TRUNCATION`], which is the benign `01004` the fetch
    /// direction posts when a caller's buffer is too small.
    ERR_PARAM_STRING_TRUNCATION = (SQLSTATE_22001, "String data, right truncation");
    WARN_STRING_TRUNCATION = (SQLSTATE_01004, "String data, right truncation");
    ERR_INVALID_NULL_POINTER = (SQLSTATE_HY009, "Invalid use of null pointer");
    ERR_INVALID_ATTRIBUTE_VALUE = (SQLSTATE_HY024, "Invalid attribute value");
    ERR_INVALID_ATTRIBUTE_IDENTIFIER = (SQLSTATE_HY092, "Invalid attribute/option identifier");
    /// `SQLEndTran` given a `CompletionType` other than `SQL_COMMIT` / `SQL_ROLLBACK`.
    ERR_INVALID_TRANSACTION_OPERATION_CODE = (SQLSTATE_HY012, "Invalid transaction operation code");
    /// `SQLDisconnect` while a manual-commit transaction holds uncommitted work
    /// (msodbcsql `sqlcconn.cpp:1234`).
    ERR_INVALID_TRANSACTION_STATE = (SQLSTATE_25000, "Invalid transaction state");
    /// Posted when turning autocommit back on commits an open transaction
    /// (msodbcsql `sqlcconn.cpp:3698`, `IDS_01_000_00`).
    WARN_TRANSACTION_COMMITTED = (
        SQLSTATE_01000,
        "The open transaction was committed because autocommit mode was enabled"
    );
    /// Client-side conversion lost a character: to the server collation for input
    /// parameters, or to the platform client code page for retrieved values.
    /// Server-side conversion loss is not observable through this diagnostic.
    ///
    /// msodbcsql's `IDS_01_000_16` verbatim (`local.rc:29`, SQLSTATE `01000` via
    /// `clntcomn.cpp:1183`). Posted only when the application opted in with
    /// `SQL_COPT_SS_WARN_ON_CP_ERROR`; see
    /// [`DbcState::warn_on_cp_error`](crate::handles::dbc::DbcState::warn_on_cp_error)
    /// for why the substitution itself is not an error (AB#47598).
    WARN_CODE_PAGE_CONVERSION_LOSS = (
        SQLSTATE_01000,
        "Warning: Code page translation caused loss of data"
    );
    /// `SQL_ATTR_TXN_ISOLATION` changed while a manual-commit transaction is open
    /// (msodbcsql `sqlcmisc.cpp:360`).
    ERR_ATTRIBUTE_CANNOT_BE_SET_NOW = (SQLSTATE_HY011, "Attribute cannot be set now");
    ERR_INVALID_CONNECTION_STRING_ATTRIBUTE = (
        SQLSTATE_01S00,
        "Invalid connection string attribute"
    );
    WARN_CANCEL_TREATED_AS_CLOSE = (SQLSTATE_01S05, "Cancel treated as FreeStmt/Close");
    ERR_INVALID_STRING_OR_BUFFER_LENGTH = (SQLSTATE_HY090, "Invalid string or buffer length");
    ERR_INVALID_INFO_TYPE = (SQLSTATE_HY096, "Information type out of range");
    /// Reported when a requested login timeout is larger than the driver accepts
    /// and is clamped. Mirrors msodbcsql's `IDS_01_S02_05` (`dll/res/local.rc:45`),
    /// which it posts from the same situation.
    WARN_LOGIN_TIMEOUT_CHANGED = (SQLSTATE_01S02, "Login timeout changed");
    /// The same clamp applied to `SQL_ATTR_CONNECTION_TIMEOUT`.
    ///
    /// A deliberate divergence: msodbcsql handles both timeouts in one arm and
    /// posts `IDS_01_S02_05` — "Login timeout changed" — for either
    /// (`sqlcmisc.cpp:1733-1741`), naming the wrong attribute when the application
    /// set the connection timeout. The SQLSTATE is what applications branch on and
    /// it is unchanged; only the human-readable text is corrected.
    WARN_CONNECTION_TIMEOUT_CHANGED = (SQLSTATE_01S02, "Connection timeout changed");
    /// `SQL_DESC_ARRAY_SIZE` clamped to `i32::MAX`. Matches msodbcsql's
    /// `IDS_01_S02` for the same clamp (`sqlcdesc.cpp:4161-4167`).
    WARN_ARRAY_SIZE_CHANGED = (SQLSTATE_01S02, "Array size changed");

    /// The same clamp applied to `SQL_ATTR_QUERY_TIMEOUT`.
    ///
    /// msodbcsql posts the generic `IDS_01_S02` here rather than one of the
    /// timeout-specific strings (`sqlcmisc.cpp:3993`), so the text matches it
    /// verbatim.
    WARN_OPTION_VALUE_CHANGED = (SQLSTATE_01S02, "Option value changed");

    /// Posted when a requested `SQL_ATTR_PACKET_SIZE` falls outside the range
    /// `mssql-tds` accepts and is clamped. Matches msodbcsql's `IDS_01_S02_02`
    /// (`dll/res/local.rc:42`), which it posts from the same clamp
    /// (`sqlcmisc.cpp:1909-1917`).
    WARN_PACKET_SIZE_CHANGED = (SQLSTATE_01S02, "Packet size changed");
}

/// All contention maps to the busy error applications retry on.
pub(crate) fn claim_error_diag(error: ClaimError) -> DiagMsg {
    match error {
        ClaimError::Busy(_) => ERR_CONNECTION_BUSY,
        ClaimError::NoClient => ERR_NO_ACTIVE_TDS_CLIENT,
    }
}

/// Post a server-originated error under a caller-chosen SQLSTATE, keeping the
/// native error number and the engine's message text.
///
/// `SQL_ATTR_CURRENT_CATALOG` needs this: msodbcsql overwrites whatever state
/// the failed `USE` produced with `IDS_HY_024` (`sqlcmisc.cpp:1873-1875`), so a
/// missing database surfaces as `HY024` + native 911 rather than the `08004`
/// the error-number map would otherwise give.
///
/// The override applies **only** to errors the engine raised. A transport or
/// protocol failure is not the server rejecting the value, so labelling a
/// dropped connection mid-`USE` as `HY024` would send the caller hunting for a
/// bad catalog name. Those take `HY000`, the general error state every other
/// execution path ([`exec_common`](super::exec_common), [`fetch`](super::fetch))
/// already uses for a failure that is not the engine's.
pub(crate) fn post_tds_error_as(
    state: &mut impl HasDiagnostics,
    err: &TdsError,
    forced_state: [u8; 5],
) {
    let TdsError::SqlServerError { diagnostics } = err else {
        post_tds_error(state, err, SQLSTATE_HY000);
        return;
    };
    if diagnostics.errors.is_empty() {
        post_tds_error(state, err, SQLSTATE_HY000);
        return;
    }
    post_server_diagnostics(
        state,
        &diagnostics.errors,
        &diagnostics.info_messages,
        &[],
        Some(forced_state),
    );
}

/// Post a driver-raised diagnostic (fixed SQLSTATE + canonical message) with
/// native error 0. For server-originated errors use [`post_tds_error`].
pub(crate) fn post_diag(state: &mut impl HasDiagnostics, msg: DiagMsg) {
    state
        .diag_records_mut()
        .push(DiagRecord::new(msg.state, 0, msg.full));
}

/// Posts `[Microsoft][ODBC Driver 18 for SQL Server][SQL Server]<message>`
/// for a diagnostic the engine raised, with its sort key.
fn post_server_message(
    state: &mut impl HasDiagnostics,
    sql_state: [u8; 5],
    native_error: i32,
    key: ServerMessageKey,
    message: &str,
) {
    post_prefixed(
        state,
        sql_state,
        native_error,
        &[SERVER_DIAG_SUBSOURCE, message],
    );
    if let Some(record) = state.diag_records_mut().last_mut() {
        record.row_number = key.row_number;
        record.priority = key.priority;
        record.arrival = key.arrival;
    }
}

/// Where a server message sorts among a call's records.
struct ServerMessageKey {
    row_number: SqlLen,
    priority: u16,
    arrival: u64,
}

impl ServerMessageKey {
    /// A parameter array's message sorts under its 1-based set, then by
    /// priority, then in arrival order, as msodbcsql files it (measured on
    /// 18.07.0001); any other keeps posting order within its priority.
    fn new(number: u32, severity: i32, parameter_set: Option<usize>, ordinal: u64) -> Self {
        let priority = server_message_priority(number, severity);
        match parameter_set {
            Some(set) => Self {
                row_number: SqlLen::try_from(set).map_or(SqlLen::MAX, |set| set.saturating_add(1)),
                priority,
                arrival: ordinal,
            },
            None => Self {
                row_number: SQL_NO_ROW_NUMBER,
                priority,
                arrival: 0,
            },
        }
    }
}

fn server_message_priority(number: u32, severity: i32) -> u16 {
    if sqlstate_for_sql_error(number).is_some() {
        0
    } else if severity <= 10 {
        40401
    } else if severity <= 18 {
        40136
    } else {
        40146
    }
}

enum ServerDiagnosticRef<'a> {
    Error(&'a SqlErrorInfo),
    Info(&'a SqlInfoMessage),
}

fn post_server_diagnostics(
    state: &mut impl HasDiagnostics,
    errors: &[SqlErrorInfo],
    info_messages: &[SqlInfoMessage],
    additional_info_messages: &[SqlInfoMessage],
    forced_error_state: Option<[u8; 5]>,
) {
    let mut ordered =
        Vec::with_capacity(errors.len() + info_messages.len() + additional_info_messages.len());
    ordered.extend(
        errors
            .iter()
            .map(|error| (error.diagnostic_ordinal, ServerDiagnosticRef::Error(error))),
    );
    ordered.extend(info_messages.iter().map(|message| {
        (
            message.diagnostic_ordinal,
            ServerDiagnosticRef::Info(message),
        )
    }));
    ordered.extend(additional_info_messages.iter().map(|message| {
        (
            message.diagnostic_ordinal,
            ServerDiagnosticRef::Info(message),
        )
    }));
    ordered.sort_by_key(|(ordinal, _)| *ordinal);

    for (ordinal, diagnostic) in ordered {
        let (number, severity, message, parameter_set, forced) = match diagnostic {
            ServerDiagnosticRef::Error(error) => (
                error.number,
                error.class,
                error.message.as_str(),
                error.parameter_set,
                forced_error_state,
            ),
            ServerDiagnosticRef::Info(message) => (
                message.number,
                message.class,
                message.message.as_str(),
                message.parameter_set,
                None,
            ),
        };
        let sqlstate = forced.unwrap_or_else(|| {
            sqlstate_for_sql_error(number).unwrap_or_else(|| sqlstate_for_severity(severity))
        });
        let native = i32::try_from(number).unwrap_or(i32::MAX);
        post_server_message(
            state,
            sqlstate,
            native,
            ServerMessageKey::new(number, severity, parameter_set, ordinal),
            message,
        );
    }
}

/// Sub-source tag msodbcsql inserts after the driver prefix for diagnostics
/// that originate inside the SQL Server engine (errors and info/PRINT
/// messages), yielding the full
/// `[Microsoft][ODBC Driver 18 for SQL Server][SQL Server]<message>`. The
/// driver prefix itself is added by [`post_server_message`].
const SERVER_DIAG_SUBSOURCE: &str = "[SQL Server]";

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

/// Highest TDS severity class that is purely informational — msodbcsql's
/// `EX_MAXISEVERITY` (`tds.h:603`).
const SEVERITY_MAX_INFO: i32 = 10;
/// Upper boundary of msodbcsql's `42000` compatibility tier —
/// `MAXUSEVERITY` (`tds.h:612`).
const SEVERITY_MAX_USER: i32 = 18;

/// SQLSTATE for a server message whose error number has no entry in
/// [`SERVER_ERROR_TO_SQL_STATE_MAP`], derived from its TDS severity class.
///
/// msodbcsql tiers this fallback rather than using one fixed state
/// (`PostSQLServerMessageEx`, `sqlcerr.cpp:1385-1401`): severity > 18 →
/// `IDS_S1_000_01` (`HY000`), > 10 → `IDS_37_000` (`42000`), otherwise
/// `IDS_01_000_00` (`01000`). The map's own header states the rule
/// (`sqlcstr.cpp:120`: "Any SQLServer error not here will receive a 37000
/// SQLState").
///
/// The middle tier is what makes an unmapped syntax error or a severity-16
/// `RAISERROR` a *programming* error rather than a generic one — the
/// distinction wrappers such as mssql-python use to pick an exception class.
/// Growing the map instead would fix one error number at a time and violate
/// its "entries are permanent" contract; the tier fixes every unmapped error.
fn sqlstate_for_severity(class: i32) -> [u8; 5] {
    if class > SEVERITY_MAX_USER {
        SQLSTATE_HY000
    } else if class > SEVERITY_MAX_INFO {
        SQLSTATE_42000
    } else {
        SQLSTATE_01000
    }
}

/// Post one ODBC diagnostic record per server error in `err`.
///
/// For [`TdsError::SqlServerError`], iterates the server-reported errors in
/// the order TDS delivered them and pushes one
/// [`DiagRecord`](crate::error::DiagRecord) each. Each record's SQLSTATE
/// comes from [`sqlstate_for_sql_error`]; any error number not in the map
/// falls back to [`sqlstate_for_severity`]. Native error and message are taken
/// straight from the server-reported error. Informational/warning messages are
/// merged with errors by their TDS arrival ordinal before lazy ODBC-priority
/// sorting, preserving chronology for equal-priority records.
///
/// For any non-`SqlServerError` variant (transport, TLS, redirect, protocol,
/// timeout, …), pushes a single record with `default`, native error 0, and
/// the error's `Display` text.
///
/// `default` therefore describes the caller's context for failures the engine
/// did *not* raise — typically `08001` for connect-time failures and `HY000`
/// for general execution / fetch failures. A server error carries its own
/// severity, so it never uses `default`.
pub(crate) fn post_tds_error(state: &mut impl HasDiagnostics, err: &TdsError, default: [u8; 5]) {
    post_tds_error_with_info(state, err, default, &[]);
}

pub(crate) fn post_tds_error_with_info(
    state: &mut impl HasDiagnostics,
    err: &TdsError,
    default: [u8; 5],
    additional_info_messages: &[SqlInfoMessage],
) {
    if let TdsError::SqlServerError { diagnostics } = err {
        if diagnostics.errors.is_empty() {
            // An error return must surface at least one error record. If the
            // server diagnostics unexpectedly carried no ERROR tokens, post a
            // default record from the error's Display text so the caller still
            // sees a primary failure.
            post_sql_error(state, default, 0, err.to_string());
            post_server_diagnostics(
                state,
                &[],
                &diagnostics.info_messages,
                additional_info_messages,
                None,
            );
        } else {
            post_server_diagnostics(
                state,
                &diagnostics.errors,
                &diagnostics.info_messages,
                additional_info_messages,
                None,
            );
        }
        return;
    }
    // An unacknowledged connection reset is a property of the link, not of the
    // statement that happened to carry the bit: `TdsClient` has already marked
    // the connection dead, so every call site must report it as a communication
    // failure regardless of the SQLSTATE its own context would suggest.
    //
    // A client-side deadline (`SQL_ATTR_QUERY_TIMEOUT` / `SQL_ATTR_CONNECTION_TIMEOUT`)
    // elapsing is likewise independent of the caller's context: ODBC reserves
    // `HYT00` specifically so an application can distinguish "my deadline
    // passed" from a generic failure, matching msodbcsql.
    let mut cause = err;
    while let TdsError::TvpCellSerialization { source, .. } = cause {
        cause = source;
    }
    let sqlstate = match cause {
        TdsError::ConnectionResetNotAcknowledged => SQLSTATE_08S01,
        TdsError::TimeoutError(_) => SQLSTATE_HYT00,
        TdsError::EncodedValueTooLong { .. } => SQLSTATE_22001,
        TdsError::OperationCancelledError(_) => SQLSTATE_HY008,
        _ => default,
    };
    post_sql_error(state, sqlstate, 0, err.to_string());
    post_tds_info_messages(state, additional_info_messages);
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
    post_server_diagnostics(state, &[], messages, &[], None);

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
            diagnostic_ordinal: 0,
            parameter_set: None,
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
        records: crate::error::DiagRecords,
    }
    impl HasDiagnostics for FakeState {
        fn diag_records(&self) -> &[crate::error::DiagRecord] {
            &self.records
        }
        fn diag_records_mut(&mut self) -> &mut crate::error::DiagRecords {
            &mut self.records
        }
    }

    #[test]
    fn post_tds_error_preserves_tvp_overflow_sqlstate_and_context() {
        let mut state = FakeState::default();
        let error = TdsError::TvpCellSerialization {
            parameter: mssql_tds::error::TvpParameterRef::OdbcOrdinal(1),
            row: 2,
            column: 3,
            source: Box::new(TdsError::EncodedValueTooLong {
                actual: 6,
                maximum: 2,
            }),
        };
        post_tds_error(&mut state, &error, SQLSTATE_HY000);
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records[0].sql_state, SQLSTATE_22001);
        assert!(
            state.records[0]
                .message
                .contains("ODBC ordinal 1, row 2, column 3")
        );
        assert!(state.records[0].message.ends_with(&error.to_string()));
    }

    #[test]
    fn post_tds_error_single_server_error_posts_one_record() {
        let mut s = FakeState::default();
        let err = TdsError::from_sql_errors(vec![sql_error(18456, "Login failed for user 'x'.")]);
        post_tds_error(&mut s, &err, SQLSTATE_08001);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, *b"28000");
        assert_eq!(s.records[0].native_error, 18456);
        assert_eq!(
            s.records[0].message,
            format!(
                "{}{SERVER_DIAG_SUBSOURCE}Login failed for user 'x'.",
                crate::error::diag::DRIVER_DIAG_PREFIX
            )
        );
    }

    #[test]
    fn post_tds_error_posts_one_record_per_server_error_in_order() {
        // 18456 → 28000 (mapped); 4060 → severity fallback (not in our map).
        // `sql_error` builds class 14, so the fallback is the middle tier —
        // `default` (08001 here) applies only to non-server failures.
        let mut s = FakeState::default();
        let err = TdsError::from_sql_errors(vec![
            sql_error(18456, "Login failed."),
            sql_error(4060, "Cannot open database 'foo'."),
        ]);
        post_tds_error(&mut s, &err, SQLSTATE_08001);
        assert_eq!(s.records.len(), 2);
        assert_eq!(s.records[0].sql_state, *b"28000");
        assert_eq!(s.records[0].native_error, 18456);
        assert_eq!(s.records[1].sql_state, SQLSTATE_42000);
        assert_eq!(s.records[1].native_error, 4060);
    }

    #[test]
    fn severity_tiers_match_msodbcsql_boundaries() {
        // sqlcerr.cpp:1385-1401 — > MAXUSEVERITY (18), > EX_MAXISEVERITY (10),
        // else. Exercise both boundaries from either side.
        assert_eq!(sqlstate_for_severity(0), SQLSTATE_01000);
        assert_eq!(sqlstate_for_severity(10), SQLSTATE_01000);
        assert_eq!(sqlstate_for_severity(11), SQLSTATE_42000);
        assert_eq!(sqlstate_for_severity(16), SQLSTATE_42000);
        assert_eq!(sqlstate_for_severity(18), SQLSTATE_42000);
        assert_eq!(sqlstate_for_severity(19), SQLSTATE_HY000);
        assert_eq!(sqlstate_for_severity(25), SQLSTATE_HY000);
    }

    #[test]
    fn unmapped_server_error_takes_sqlstate_from_its_severity() {
        // The three cases that drove AB#47532: an unmapped syntax error (102,
        // severity 15), an unmapped conversion error (257, severity 16) and
        // RAISERROR's generic 50000. None is in the map — nor in msodbcsql's —
        // yet all must reach 42000 so wrappers classify them as programming
        // errors rather than operational ones.
        for (number, class) in [(102u32, 15), (257, 16), (50000, 16)] {
            assert_eq!(sqlstate_for_sql_error(number), None, "error {number}");
            let mut s = FakeState::default();
            let mut e = sql_error(number, "boom");
            e.class = class;
            post_tds_error(&mut s, &TdsError::from_sql_errors(vec![e]), SQLSTATE_HY000);
            assert_eq!(s.records.len(), 1);
            assert_eq!(s.records[0].sql_state, SQLSTATE_42000, "error {number}");
        }
    }

    #[test]
    fn unmapped_fatal_server_error_stays_hy000() {
        // Severity above MAXUSEVERITY is not something the application can fix
        // by rewriting its statement, so it keeps the general error state.
        let mut s = FakeState::default();
        let mut e = sql_error(823, "I/O error");
        e.class = 24;
        post_tds_error(&mut s, &TdsError::from_sql_errors(vec![e]), SQLSTATE_HY000);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(s.records[0].native_error, 823);
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
                diagnostic_ordinal: 0,
                parameter_set: None,
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
    fn server_diagnostics_sort_by_priority_then_wire_order() {
        use mssql_tds::error::SqlServerDiagnostics;

        let error = SqlErrorInfo {
            diagnostic_ordinal: 1,
            parameter_set: None,
            message: "unmapped server error".into(),
            state: 1,
            class: 16,
            number: 50000,
            server_name: None,
            proc_name: None,
            line_number: Some(2),
        };
        let info = SqlInfoMessage {
            diagnostic_ordinal: 0,
            parameter_set: None,
            message: "truncated value".into(),
            state: 1,
            class: 10,
            number: 8153,
            server_name: None,
            proc_name: None,
            line_number: Some(1),
        };
        let err =
            TdsError::from_sql_diagnostics(SqlServerDiagnostics::new(vec![error], vec![info]));
        let mut state = FakeState::default();

        post_tds_error(&mut state, &err, SQLSTATE_HY000);
        crate::error::sort_diagnostics(&mut state);

        assert_eq!(state.records[0].sql_state, *b"01003");
        assert_eq!(state.records[0].native_error, 8153);
        assert_eq!(state.records[1].sql_state, SQLSTATE_42000);
        assert_eq!(state.records[1].native_error, 50000);
    }

    #[test]
    fn equal_priority_server_diagnostics_keep_wire_order() {
        use mssql_tds::error::SqlServerDiagnostics;

        let mut error = sql_error(207, "invalid column");
        error.diagnostic_ordinal = 1;
        let info = SqlInfoMessage {
            diagnostic_ordinal: 0,
            parameter_set: None,
            message: "first warning".into(),
            state: 1,
            class: 10,
            number: 8153,
            server_name: None,
            proc_name: None,
            line_number: Some(1),
        };
        let err =
            TdsError::from_sql_diagnostics(SqlServerDiagnostics::new(vec![error], vec![info]));
        let mut state = FakeState::default();

        post_tds_error(&mut state, &err, SQLSTATE_HY000);
        crate::error::sort_diagnostics(&mut state);

        assert_eq!(state.records[0].native_error, 8153);
        assert_eq!(state.records[1].native_error, 207);
    }

    /// A parameter array's server messages sort set by set, then by priority,
    /// then in arrival order: set 1's unmapped error stays ahead of set 2's
    /// mapped one, and set 2's mapped warning, which arrived first, stays ahead
    /// of its error although the error is posted first (msodbcsql 18.07.0001).
    #[test]
    fn server_diagnostics_sort_by_parameter_set_before_priority() {
        let message = |ordinal, set, number, class: i32| SqlInfoMessage {
            diagnostic_ordinal: ordinal,
            parameter_set: Some(set),
            message: "message".into(),
            state: 1,
            class,
            number,
            server_name: None,
            proc_name: None,
            line_number: Some(1),
        };
        let error = |ordinal, set, number| SqlErrorInfo {
            diagnostic_ordinal: ordinal,
            parameter_set: Some(set),
            class: 16,
            ..sql_error(number, "failure")
        };
        let mut state = FakeState::default();

        for errors in [vec![error(1, 0, 50000)], vec![error(3, 1, 2627)]] {
            post_tds_error(
                &mut state,
                &TdsError::from_sql_errors(errors),
                SQLSTATE_HY000,
            );
        }
        post_tds_info_messages(
            &mut state,
            &[
                message(0, 0, 50001, 10),
                message(2, 1, 8153, 10),
                message(4, 1, 3621, 0),
            ],
        );
        crate::error::sort_diagnostics(&mut state);

        let order: Vec<_> = state
            .records
            .iter()
            .map(|record| (record.row_number, record.native_error))
            .collect();
        assert_eq!(
            order,
            [(1, 50000), (1, 50001), (2, 8153), (2, 2627), (2, 3621)]
        );
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
                diagnostic_ordinal: 0,
                parameter_set: None,
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
    fn encoded_value_overflow_maps_to_22001() {
        let mut s = FakeState::default();
        let err = TdsError::EncodedValueTooLong {
            actual: 5,
            maximum: 3,
        };
        post_tds_error(&mut s, &err, SQLSTATE_HY000);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, SQLSTATE_22001);
        assert_eq!(s.records[0].native_error, 0);
    }

    /// A client-side deadline elapsing (`SQL_ATTR_QUERY_TIMEOUT` /
    /// `SQL_ATTR_CONNECTION_TIMEOUT`) must report `HYT00` regardless of the
    /// caller's `default`, matching msodbcsql — see mssql-rs#439.
    #[test]
    fn post_tds_error_timeout_maps_to_hyt00_regardless_of_default() {
        use mssql_tds::error::TimeoutErrorType;

        let mut s = FakeState::default();
        let err = TdsError::TimeoutError(TimeoutErrorType::String("deadline exceeded".into()));
        post_tds_error(&mut s, &err, SQLSTATE_HY000);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, SQLSTATE_HYT00);
        assert_eq!(s.records[0].native_error, 0);
    }

    #[test]
    fn post_tds_error_cancel_maps_to_hy008_regardless_of_default() {
        let mut s = FakeState::default();
        let err = TdsError::OperationCancelledError("Request was cancelled".to_string());
        post_tds_error(&mut s, &err, SQLSTATE_HY000);
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records[0].sql_state, SQLSTATE_HY008);
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

    /// Runs `post_tds_error_as` against a real [`DbcState`] — the only type the
    /// driver instantiates it with — and returns what it posted.
    fn post_as_on_dbc(err: &TdsError, forced: [u8; 5]) -> Vec<crate::error::DiagRecord> {
        use crate::handles::dbc::DbcHandle;
        use crate::handles::handle_from_raw;
        use crate::test_support::TestHandles;

        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut state = dbc.inner.lock().unwrap();
        post_tds_error_as(&mut *state, err, forced);
        state.diag_records.clone()
    }

    #[test]
    fn post_tds_error_as_overrides_the_error_number_mapping() {
        // 911 maps to 08004 by error number, but the catalog path forces
        // HY024 the way msodbcsql does (`sqlcmisc.cpp:1873-1875`). The native
        // number and the engine's text must survive the override.
        assert_eq!(sqlstate_for_sql_error(911), Some(*b"08004"));
        let err = TdsError::from_sql_errors(vec![sql_error(
            911,
            "Database 'nope' does not exist. Make sure that the name is entered correctly.",
        )]);
        let records = post_as_on_dbc(&err, SQLSTATE_HY024);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sql_state, SQLSTATE_HY024);
        assert_eq!(records[0].native_error, 911);
        assert!(records[0].message.ends_with(
            "Database 'nope' does not exist. Make sure that the name is entered correctly."
        ));
    }

    #[test]
    fn post_tds_error_as_forces_every_error_then_appends_info_messages() {
        use mssql_tds::error::SqlServerDiagnostics;
        let diagnostics = SqlServerDiagnostics::new(
            vec![sql_error(911, "no such db"), sql_error(5701, "context")],
            vec![SqlInfoMessage {
                diagnostic_ordinal: 0,
                parameter_set: None,
                message: "Changed database context to 'master'.".into(),
                state: 1,
                class: 10,
                number: 5701,
                server_name: None,
                proc_name: None,
                line_number: None,
            }],
        );
        let records = post_as_on_dbc(&TdsError::from_sql_diagnostics(diagnostics), SQLSTATE_HY024);
        assert_eq!(records.len(), 3);
        // Every server error takes the forced state, in order.
        assert_eq!(records[0].sql_state, SQLSTATE_HY024);
        assert_eq!(records[1].sql_state, SQLSTATE_HY024);
        // Info messages keep their own 01000 state and are appended last.
        assert_eq!(records[2].sql_state, *b"01000");
        assert_eq!(records[2].native_error, 5701);
    }

    #[test]
    fn post_tds_error_as_does_not_force_the_state_on_a_transport_failure() {
        // A dropped connection mid-`USE` is not the engine rejecting the
        // catalog name, so it must not inherit HY024 — a caller would go
        // hunting for a bad database name that was never the problem.
        let records = post_as_on_dbc(
            &TdsError::ProtocolError("bad packet".into()),
            SQLSTATE_HY024,
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(records[0].native_error, 0);
        assert!(records[0].message.contains("bad packet"));
    }

    #[test]
    fn post_tds_error_as_falls_back_when_the_error_list_is_empty() {
        // No engine error to force the state onto, so the general default wins.
        let records = post_as_on_dbc(&TdsError::from_sql_errors(vec![]), SQLSTATE_HY024);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(records[0].native_error, 0);
    }

    #[test]
    fn post_tds_info_messages_posts_records_and_reports_presence() {
        let mut s = FakeState::default();
        let messages = vec![
            SqlInfoMessage {
                diagnostic_ordinal: 0,
                parameter_set: None,
                message: "Changed database context to 'master'.".into(),
                state: 1,
                class: 10,
                number: 5701,
                server_name: Some("server".into()),
                proc_name: None,
                line_number: Some(1),
            },
            SqlInfoMessage {
                diagnostic_ordinal: 0,
                parameter_set: None,
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
            format!(
                "{}{SERVER_DIAG_SUBSOURCE}Changed database context to 'master'.",
                crate::error::diag::DRIVER_DIAG_PREFIX
            )
        );
        assert_eq!(s.records[1].sql_state, SQLSTATE_01000);
        assert_eq!(s.records[1].native_error, 0);
        assert_eq!(
            s.records[1].message,
            format!(
                "{}{SERVER_DIAG_SUBSOURCE}hello from PRINT",
                crate::error::diag::DRIVER_DIAG_PREFIX
            )
        );
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
        assert_eq!(
            s.records[0].message,
            format!(
                "{}Connection does not exist",
                crate::error::diag::DRIVER_DIAG_PREFIX
            )
        );
    }

    #[test]
    fn driver_raised_error_carries_only_driver_prefix() {
        let mut s = FakeState::default();
        post_sql_error(&mut s, SQLSTATE_HY000, 0, "Something broke");
        assert_eq!(
            s.records[0].message,
            "[Microsoft][ODBC Driver 18 for SQL Server]Something broke"
        );
    }

    #[test]
    fn server_error_carries_driver_and_sql_server_prefix() {
        let mut s = FakeState::default();
        let err = TdsError::from_sql_errors(vec![sql_error(18456, "Login failed for user 'x'.")]);
        post_tds_error(&mut s, &err, SQLSTATE_08001);
        assert_eq!(
            s.records[0].message,
            "[Microsoft][ODBC Driver 18 for SQL Server][SQL Server]Login failed for user 'x'."
        );
    }

    #[test]
    fn forced_state_server_error_keeps_full_server_text() {
        let err =
            TdsError::from_sql_errors(vec![sql_error(911, "Database 'nope' does not exist.")]);
        let records = post_as_on_dbc(&err, SQLSTATE_HY024);
        assert_eq!(
            records[0].message,
            "[Microsoft][ODBC Driver 18 for SQL Server][SQL Server]Database 'nope' does not exist."
        );
    }

    #[test]
    fn non_server_error_text_is_prefixed_display() {
        let mut s = FakeState::default();
        let err = TdsError::ProtocolError("bad packet".into());
        post_tds_error(&mut s, &err, SQLSTATE_HY000);
        assert_eq!(
            s.records[0].message,
            format!("[Microsoft][ODBC Driver 18 for SQL Server]{err}")
        );
    }

    #[test]
    fn every_diag_msg_full_text_is_prefix_plus_text() {
        use crate::conversion::error::ConvError;

        assert!(
            ALL_DIAG_MSGS
                .iter()
                .any(|(name, _)| *name == "ERR_COLUMN_CONVERSION_NOT_IMPLEMENTED")
        );
        let not_handled = DiagMsg::from(ConvError::NotHandledHere);
        let all = ALL_DIAG_MSGS
            .iter()
            .copied()
            .chain([("ConvError::NotHandledHere", not_handled)]);
        for (name, msg) in all {
            assert_eq!(
                msg.full,
                format!("[Microsoft][ODBC Driver 18 for SQL Server]{}", msg.text),
                "{name}"
            );
        }
    }

    #[test]
    fn post_diag_borrows_static_text() {
        for &(name, msg) in ALL_DIAG_MSGS {
            let mut s = FakeState::default();
            post_diag(&mut s, msg);
            let [record] = s.records.as_slice() else {
                panic!("{name}: expected one record");
            };
            assert_eq!(record.sql_state, msg.state, "{name}");
            assert_eq!(record.native_error, 0, "{name}");
            let std::borrow::Cow::Borrowed(text) = record.message else {
                panic!("{name}: message was allocated");
            };
            assert!(std::ptr::eq(text, msg.full), "{name}");
        }
    }
}
