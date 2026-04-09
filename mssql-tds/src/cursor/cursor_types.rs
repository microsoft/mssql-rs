// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cursor type definitions for TDS cursor RPCs.
//!
//! All types correspond to parameter flags used by the `sp_cursor*` family of
//! system stored procedures (RPC IDs 1–9 in TDS). See the
//! [MS-TDS specification](https://learn.microsoft.com/openspecs/windows_protocols/ms-tds)
//! and the individual `sp_cursor*` reference pages on Microsoft Learn.

use bitflags::bitflags;

bitflags! {
    /// Cursor scrollability and behavior flags for the `scrollopt` parameter
    /// of `sp_cursoropen`, `sp_cursorprepare`, `sp_cursorexecute`, and
    /// `sp_cursorprepexec`.
    ///
    /// Combine a base cursor type with modifier and acceptability flags:
    ///
    /// ```
    /// use mssql_tds::cursor::CursorScrollOption;
    ///
    /// let opts = CursorScrollOption::FORWARD_ONLY
    ///     | CursorScrollOption::AUTO_FETCH
    ///     | CursorScrollOption::AUTO_CLOSE;
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CursorScrollOption: u32 {
        // ── Base cursor types (mutually exclusive) ──────────────
        /// Keyset-driven cursor. Membership fixed at open time; row values
        /// reflect concurrent updates and deletes by other sessions.
        const KEYSET_DRIVEN        = 0x0000_0001;
        /// Dynamic cursor. Every fetch re-evaluates the query. All external
        /// changes (inserts, updates, deletes) are visible.
        const DYNAMIC              = 0x0000_0002;
        /// Forward-only cursor. Can only fetch NEXT.
        const FORWARD_ONLY         = 0x0000_0004;
        /// Static (insensitive) cursor. Server creates a tempdb snapshot;
        /// no visibility to changes after open.
        const STATIC               = 0x0000_0008;
        /// Fast forward-only cursor. Server-optimized read-only forward path.
        const FAST_FORWARD_ONLY    = 0x0000_0010;

        // ── Modifier flags ──────────────────────────────────────
        /// Statement contains parameter markers (`@p1`, etc.).
        const PARAMETERIZED_STMT   = 0x0000_1000;
        /// Return the first rowset inline with the open response.
        const AUTO_FETCH           = 0x0000_2000;
        /// Automatically close the cursor when all rows are consumed.
        const AUTO_CLOSE           = 0x0000_4000;
        /// Enable cursor-type negotiation. The server may downgrade the
        /// requested type if acceptability flags are set.
        const CHECK_ACCEPTED_TYPES = 0x0000_8000;

        // ── Acceptability flags (allow server downgrades) ───────
        /// Allow downgrade to keyset-driven.
        const KEYSET_ACCEPTABLE       = 0x0001_0000;
        /// Allow downgrade to dynamic.
        const DYNAMIC_ACCEPTABLE      = 0x0002_0000;
        /// Allow downgrade to forward-only.
        const FORWARD_ONLY_ACCEPTABLE = 0x0004_0000;
        /// Allow downgrade to static.
        const STATIC_ACCEPTABLE       = 0x0008_0000;
        /// Allow downgrade to fast forward-only.
        const FAST_FORWARD_ACCEPTABLE = 0x0010_0000;
    }
}

bitflags! {
    /// Cursor concurrency flags for the `ccopt` parameter of `sp_cursoropen`,
    /// `sp_cursorprepare`, `sp_cursorexecute`, and `sp_cursorprepexec`.
    ///
    /// ```
    /// use mssql_tds::cursor::CursorConcurrency;
    ///
    /// let cc = CursorConcurrency::OPTCC
    ///     | CursorConcurrency::CHECK_ACCEPTED_OPTS
    ///     | CursorConcurrency::READ_ONLY_ACCEPTABLE;
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CursorConcurrency: u32 {
        // ── Base concurrency modes (mutually exclusive) ─────────
        /// Read-only. No updates allowed through the cursor.
        const READONLY             = 0x0000_0001;
        /// Pessimistic locking. Rows are locked when fetched.
        const LOCKCC               = 0x0000_0002;
        /// Optimistic concurrency using a `rowversion`/`timestamp` column.
        const OPTCC                = 0x0000_0004;
        /// Optimistic concurrency comparing all column values.
        const OPTCCVAL             = 0x0000_0008;

        // ── Modifier flags ──────────────────────────────────────
        /// Allow non-SELECT statements (e.g., INSERT, stored procedures).
        const ALLOW_DIRECT         = 0x0000_2000;
        /// Update keyset in place rather than rebuilding.
        const UPDATE_KEYSET_INPLACE = 0x0000_4000;
        /// Enable concurrency negotiation. The server may downgrade if
        /// acceptability flags are set.
        const CHECK_ACCEPTED_OPTS  = 0x0000_8000;

        // ── Acceptability flags (allow server downgrades) ───────
        /// Allow downgrade to read-only.
        const READ_ONLY_ACCEPTABLE = 0x0001_0000;
        /// Allow downgrade to pessimistic locking.
        const LOCKS_ACCEPTABLE     = 0x0002_0000;
        /// Allow downgrade to optimistic concurrency.
        const OPTIMISTIC_ACCEPTABLE = 0x0004_0000;
    }
}

bitflags! {
    /// Fetch direction flags for the `fetchtype` parameter of `sp_cursorfetch`.
    ///
    /// Forward-only cursors support only [`NEXT`](Self::NEXT). Scrollable
    /// cursors (static, keyset, dynamic) support all directions.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FetchDirection: u32 {
        /// Move to the first row.
        const FIRST          = 0x0000_0001;
        /// Move to the next row (forward).
        const NEXT           = 0x0000_0002;
        /// Move to the previous row (backward).
        const PREV           = 0x0000_0004;
        /// Move to the last row.
        const LAST           = 0x0000_0008;
        /// Move to the row at absolute position N.
        const ABSOLUTE       = 0x0000_0010;
        /// Move N rows from the current position.
        const RELATIVE       = 0x0000_0020;
        /// Re-fetch the current row(s).
        const REFRESH        = 0x0000_0080;
        /// Information-only fetch (no data returned).
        const INFO           = 0x0000_0100;
        /// Like PREV but without position adjustment.
        const PREV_NOADJUST  = 0x0000_0200;
        /// Skip concurrency timestamp update (combine with other directions).
        const SKIP_UPDT_CNCY = 0x0000_0400;
    }
}

bitflags! {
    /// Operation type flags for the `optype` parameter of `sp_cursor`
    /// (positioned operations on an open cursor).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CursorOperation: u32 {
        /// Positioned UPDATE on the current row.
        const UPDATE       = 0x0000_0001;
        /// Positioned DELETE on the current row.
        const DELETE       = 0x0000_0002;
        /// INSERT through the cursor.
        const INSERT       = 0x0000_0004;
        /// Refresh the current row position.
        const REFRESH_POS  = 0x0000_0008;
        /// Lock the current row.
        const LOCK         = 0x0000_0010;
        /// Set position to a specific row number within the rowset.
        const SET_POSITION = 0x0000_0020;
        /// Set position by bookmark/absolute.
        const SET_ABSOLUTE = 0x0000_0040;
    }
}

bitflags! {
    /// Per-row fetch status returned by `sp_cursorfetch` for keyset and
    /// dynamic cursors. Indicates the state of each row relative to the
    /// original keyset.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FetchStatus: u32 {
        /// Row fetched successfully.
        const SUCCEEDED      = 0x0000_0001;
        /// Row has been deleted since the keyset was built.
        const MISSING        = 0x0000_0002;
        /// Position is past the end of the keyset.
        const END_OF_KEYSET  = 0x0000_0004;
        /// Position is past the end of the result set (includes END_OF_KEYSET).
        const END_OF_RESULTS = 0x0000_000C;
        /// Row was inserted since the keyset was built.
        const ADDED          = 0x0000_0010;
        /// Row was updated since the keyset was built.
        const UPDATED        = 0x0000_0020;
    }
}

/// Return status from cursor RPCs (`sp_cursoropen`, `sp_cursorprepexec`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum CursorStatus {
    /// Operation completed successfully.
    Succeeded = 0,
    /// Operation failed.
    Failed = 1,
    /// Asynchronous keyset generation in progress.
    Async = 2,
    /// Cursor was auto-closed (empty result or end of results).
    Closed = 16,
}

impl CursorStatus {
    /// Convert a raw `i32` return status to a `CursorStatus`.
    ///
    /// Returns `None` for unrecognized values.
    pub fn from_raw(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Succeeded),
            1 => Some(Self::Failed),
            2 => Some(Self::Async),
            16 => Some(Self::Closed),
            _ => None,
        }
    }
}

/// Option codes for the `code` parameter of `sp_cursoroption`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum CursorOptionCode {
    /// Text pointer handling mode (value: INT).
    TextPtrOnly = 1,
    /// Assign a name to the cursor (value: NVARCHAR).
    CursorName = 2,
    /// Inline text/image data mode (value: INT).
    TextData = 3,
    /// Query current scroll option value (value: INT).
    ScrollOpt = 4,
    /// Query current concurrency option value (value: INT).
    CcOpt = 5,
    /// Query current result set row count (value: INT).
    RowCount = 6,
}

impl CursorOptionCode {
    /// Convert a raw `i32` to a `CursorOptionCode`.
    ///
    /// Returns `None` for unrecognized values.
    pub fn from_raw(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::TextPtrOnly),
            2 => Some(Self::CursorName),
            3 => Some(Self::TextData),
            4 => Some(Self::ScrollOpt),
            5 => Some(Self::CcOpt),
            6 => Some(Self::RowCount),
            _ => None,
        }
    }

    /// Returns `true` if this option code expects a string value.
    pub fn expects_string(&self) -> bool {
        matches!(self, Self::CursorName)
    }
}

/// Typed value for `sp_cursoroption`. The option code determines which
/// variant is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorOptionValue {
    /// Integer option value (used by all codes except `CursorName`).
    Int(i32),
    /// String option value (used only by `CursorName`).
    String(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CursorScrollOption ──────────────────────────────────────────

    #[test]
    fn scroll_option_base_type_values() {
        assert_eq!(CursorScrollOption::KEYSET_DRIVEN.bits(), 0x0000_0001);
        assert_eq!(CursorScrollOption::DYNAMIC.bits(), 0x0000_0002);
        assert_eq!(CursorScrollOption::FORWARD_ONLY.bits(), 0x0000_0004);
        assert_eq!(CursorScrollOption::STATIC.bits(), 0x0000_0008);
        assert_eq!(CursorScrollOption::FAST_FORWARD_ONLY.bits(), 0x0000_0010);
    }

    #[test]
    fn scroll_option_modifier_values() {
        assert_eq!(CursorScrollOption::PARAMETERIZED_STMT.bits(), 0x0000_1000);
        assert_eq!(CursorScrollOption::AUTO_FETCH.bits(), 0x0000_2000);
        assert_eq!(CursorScrollOption::AUTO_CLOSE.bits(), 0x0000_4000);
        assert_eq!(CursorScrollOption::CHECK_ACCEPTED_TYPES.bits(), 0x0000_8000);
    }

    #[test]
    fn scroll_option_acceptability_values() {
        assert_eq!(CursorScrollOption::KEYSET_ACCEPTABLE.bits(), 0x0001_0000);
        assert_eq!(CursorScrollOption::DYNAMIC_ACCEPTABLE.bits(), 0x0002_0000);
        assert_eq!(
            CursorScrollOption::FORWARD_ONLY_ACCEPTABLE.bits(),
            0x0004_0000
        );
        assert_eq!(CursorScrollOption::STATIC_ACCEPTABLE.bits(), 0x0008_0000);
        assert_eq!(
            CursorScrollOption::FAST_FORWARD_ACCEPTABLE.bits(),
            0x0010_0000
        );
    }

    #[test]
    fn scroll_option_combined_flags() {
        let opts = CursorScrollOption::FORWARD_ONLY
            | CursorScrollOption::AUTO_FETCH
            | CursorScrollOption::AUTO_CLOSE;
        assert_eq!(opts.bits(), 0x6004);
        assert!(opts.contains(CursorScrollOption::FORWARD_ONLY));
        assert!(opts.contains(CursorScrollOption::AUTO_FETCH));
        assert!(opts.contains(CursorScrollOption::AUTO_CLOSE));
        assert!(!opts.contains(CursorScrollOption::DYNAMIC));
    }

    #[test]
    fn scroll_option_with_negotiation() {
        let opts = CursorScrollOption::DYNAMIC
            | CursorScrollOption::CHECK_ACCEPTED_TYPES
            | CursorScrollOption::KEYSET_ACCEPTABLE
            | CursorScrollOption::STATIC_ACCEPTABLE;
        assert_eq!(opts.bits(), 0x0009_8002);
        assert!(opts.contains(CursorScrollOption::DYNAMIC));
        assert!(opts.contains(CursorScrollOption::CHECK_ACCEPTED_TYPES));
        assert!(opts.contains(CursorScrollOption::KEYSET_ACCEPTABLE));
        assert!(opts.contains(CursorScrollOption::STATIC_ACCEPTABLE));
    }

    #[test]
    fn scroll_option_from_bits_truncate() {
        let raw: u32 = 0x0000_2004; // FORWARD_ONLY | AUTO_FETCH
        let opts = CursorScrollOption::from_bits_truncate(raw);
        assert!(opts.contains(CursorScrollOption::FORWARD_ONLY));
        assert!(opts.contains(CursorScrollOption::AUTO_FETCH));
    }

    #[test]
    fn scroll_option_from_bits_truncate_unknown() {
        // Unknown bits should be silently dropped
        let raw: u32 = 0xFF00_0004; // FORWARD_ONLY + garbage high bits
        let opts = CursorScrollOption::from_bits_truncate(raw);
        assert!(opts.contains(CursorScrollOption::FORWARD_ONLY));
        assert_eq!(opts.bits(), 0x0000_0004);
    }

    #[test]
    fn scroll_option_empty() {
        let opts = CursorScrollOption::empty();
        assert_eq!(opts.bits(), 0);
        assert!(!opts.contains(CursorScrollOption::FORWARD_ONLY));
    }

    // ── CursorConcurrency ───────────────────────────────────────────

    #[test]
    fn concurrency_base_mode_values() {
        assert_eq!(CursorConcurrency::READONLY.bits(), 0x0000_0001);
        assert_eq!(CursorConcurrency::LOCKCC.bits(), 0x0000_0002);
        assert_eq!(CursorConcurrency::OPTCC.bits(), 0x0000_0004);
        assert_eq!(CursorConcurrency::OPTCCVAL.bits(), 0x0000_0008);
    }

    #[test]
    fn concurrency_modifier_values() {
        assert_eq!(CursorConcurrency::ALLOW_DIRECT.bits(), 0x0000_2000);
        assert_eq!(CursorConcurrency::UPDATE_KEYSET_INPLACE.bits(), 0x0000_4000);
        assert_eq!(CursorConcurrency::CHECK_ACCEPTED_OPTS.bits(), 0x0000_8000);
    }

    #[test]
    fn concurrency_acceptability_values() {
        assert_eq!(CursorConcurrency::READ_ONLY_ACCEPTABLE.bits(), 0x0001_0000);
        assert_eq!(CursorConcurrency::LOCKS_ACCEPTABLE.bits(), 0x0002_0000);
        assert_eq!(CursorConcurrency::OPTIMISTIC_ACCEPTABLE.bits(), 0x0004_0000);
    }

    #[test]
    fn concurrency_combined_flags() {
        let cc = CursorConcurrency::OPTCC
            | CursorConcurrency::CHECK_ACCEPTED_OPTS
            | CursorConcurrency::READ_ONLY_ACCEPTABLE;
        assert_eq!(cc.bits(), 0x0001_8004);
        assert!(cc.contains(CursorConcurrency::OPTCC));
        assert!(cc.contains(CursorConcurrency::CHECK_ACCEPTED_OPTS));
        assert!(cc.contains(CursorConcurrency::READ_ONLY_ACCEPTABLE));
    }

    #[test]
    fn concurrency_from_bits_truncate() {
        let raw: u32 = 0x0000_0001; // READONLY
        let cc = CursorConcurrency::from_bits_truncate(raw);
        assert!(cc.contains(CursorConcurrency::READONLY));
    }

    // ── FetchDirection ──────────────────────────────────────────────

    #[test]
    fn fetch_direction_values() {
        assert_eq!(FetchDirection::FIRST.bits(), 0x0000_0001);
        assert_eq!(FetchDirection::NEXT.bits(), 0x0000_0002);
        assert_eq!(FetchDirection::PREV.bits(), 0x0000_0004);
        assert_eq!(FetchDirection::LAST.bits(), 0x0000_0008);
        assert_eq!(FetchDirection::ABSOLUTE.bits(), 0x0000_0010);
        assert_eq!(FetchDirection::RELATIVE.bits(), 0x0000_0020);
        assert_eq!(FetchDirection::REFRESH.bits(), 0x0000_0080);
        assert_eq!(FetchDirection::INFO.bits(), 0x0000_0100);
        assert_eq!(FetchDirection::PREV_NOADJUST.bits(), 0x0000_0200);
        assert_eq!(FetchDirection::SKIP_UPDT_CNCY.bits(), 0x0000_0400);
    }

    #[test]
    fn fetch_direction_combined_skip_update() {
        let dir = FetchDirection::NEXT | FetchDirection::SKIP_UPDT_CNCY;
        assert_eq!(dir.bits(), 0x0000_0402);
        assert!(dir.contains(FetchDirection::NEXT));
        assert!(dir.contains(FetchDirection::SKIP_UPDT_CNCY));
    }

    // ── CursorOperation ────────────────────────────────────────────

    #[test]
    fn cursor_operation_values() {
        assert_eq!(CursorOperation::UPDATE.bits(), 0x0000_0001);
        assert_eq!(CursorOperation::DELETE.bits(), 0x0000_0002);
        assert_eq!(CursorOperation::INSERT.bits(), 0x0000_0004);
        assert_eq!(CursorOperation::REFRESH_POS.bits(), 0x0000_0008);
        assert_eq!(CursorOperation::LOCK.bits(), 0x0000_0010);
        assert_eq!(CursorOperation::SET_POSITION.bits(), 0x0000_0020);
        assert_eq!(CursorOperation::SET_ABSOLUTE.bits(), 0x0000_0040);
    }

    #[test]
    fn cursor_operation_combined() {
        let op = CursorOperation::UPDATE | CursorOperation::SET_POSITION;
        assert_eq!(op.bits(), 0x0000_0021);
        assert!(op.contains(CursorOperation::UPDATE));
        assert!(op.contains(CursorOperation::SET_POSITION));
    }

    // ── FetchStatus ────────────────────────────────────────────────

    #[test]
    fn fetch_status_values() {
        assert_eq!(FetchStatus::SUCCEEDED.bits(), 0x0000_0001);
        assert_eq!(FetchStatus::MISSING.bits(), 0x0000_0002);
        assert_eq!(FetchStatus::END_OF_KEYSET.bits(), 0x0000_0004);
        assert_eq!(FetchStatus::END_OF_RESULTS.bits(), 0x0000_000C);
        assert_eq!(FetchStatus::ADDED.bits(), 0x0000_0010);
        assert_eq!(FetchStatus::UPDATED.bits(), 0x0000_0020);
    }

    #[test]
    fn fetch_status_end_of_results_includes_end_of_keyset() {
        // END_OF_RESULTS (0x0C) is a superset of END_OF_KEYSET (0x04)
        let status = FetchStatus::END_OF_RESULTS;
        assert!(status.contains(FetchStatus::END_OF_KEYSET));
    }

    #[test]
    fn fetch_status_from_bits_truncate() {
        let raw: u32 = 0x0000_0021; // SUCCEEDED | UPDATED
        let status = FetchStatus::from_bits_truncate(raw);
        assert!(status.contains(FetchStatus::SUCCEEDED));
        assert!(status.contains(FetchStatus::UPDATED));
    }

    // ── CursorStatus ───────────────────────────────────────────────

    #[test]
    fn cursor_status_from_raw_valid() {
        assert_eq!(CursorStatus::from_raw(0), Some(CursorStatus::Succeeded));
        assert_eq!(CursorStatus::from_raw(1), Some(CursorStatus::Failed));
        assert_eq!(CursorStatus::from_raw(2), Some(CursorStatus::Async));
        assert_eq!(CursorStatus::from_raw(16), Some(CursorStatus::Closed));
    }

    #[test]
    fn cursor_status_from_raw_invalid() {
        assert_eq!(CursorStatus::from_raw(-1), None);
        assert_eq!(CursorStatus::from_raw(3), None);
        assert_eq!(CursorStatus::from_raw(99), None);
    }

    #[test]
    fn cursor_status_discriminant_values() {
        assert_eq!(CursorStatus::Succeeded as i32, 0);
        assert_eq!(CursorStatus::Failed as i32, 1);
        assert_eq!(CursorStatus::Async as i32, 2);
        assert_eq!(CursorStatus::Closed as i32, 16);
    }

    // ── CursorOptionCode ───────────────────────────────────────────

    #[test]
    fn cursor_option_code_from_raw_valid() {
        assert_eq!(
            CursorOptionCode::from_raw(1),
            Some(CursorOptionCode::TextPtrOnly)
        );
        assert_eq!(
            CursorOptionCode::from_raw(2),
            Some(CursorOptionCode::CursorName)
        );
        assert_eq!(
            CursorOptionCode::from_raw(3),
            Some(CursorOptionCode::TextData)
        );
        assert_eq!(
            CursorOptionCode::from_raw(4),
            Some(CursorOptionCode::ScrollOpt)
        );
        assert_eq!(CursorOptionCode::from_raw(5), Some(CursorOptionCode::CcOpt));
        assert_eq!(
            CursorOptionCode::from_raw(6),
            Some(CursorOptionCode::RowCount)
        );
    }

    #[test]
    fn cursor_option_code_from_raw_invalid() {
        assert_eq!(CursorOptionCode::from_raw(0), None);
        assert_eq!(CursorOptionCode::from_raw(7), None);
        assert_eq!(CursorOptionCode::from_raw(-1), None);
    }

    #[test]
    fn cursor_option_code_expects_string() {
        assert!(!CursorOptionCode::TextPtrOnly.expects_string());
        assert!(CursorOptionCode::CursorName.expects_string());
        assert!(!CursorOptionCode::TextData.expects_string());
        assert!(!CursorOptionCode::ScrollOpt.expects_string());
        assert!(!CursorOptionCode::CcOpt.expects_string());
        assert!(!CursorOptionCode::RowCount.expects_string());
    }

    #[test]
    fn cursor_option_code_discriminant_values() {
        assert_eq!(CursorOptionCode::TextPtrOnly as i32, 1);
        assert_eq!(CursorOptionCode::CursorName as i32, 2);
        assert_eq!(CursorOptionCode::TextData as i32, 3);
        assert_eq!(CursorOptionCode::ScrollOpt as i32, 4);
        assert_eq!(CursorOptionCode::CcOpt as i32, 5);
        assert_eq!(CursorOptionCode::RowCount as i32, 6);
    }

    // ── CursorOptionValue ──────────────────────────────────────────

    #[test]
    fn cursor_option_value_int() {
        let val = CursorOptionValue::Int(42);
        assert_eq!(val, CursorOptionValue::Int(42));
        assert_ne!(val, CursorOptionValue::Int(0));
        assert_ne!(val, CursorOptionValue::String("42".into()));
    }

    #[test]
    fn cursor_option_value_string() {
        let val = CursorOptionValue::String("my_cursor".into());
        assert_eq!(val, CursorOptionValue::String("my_cursor".into()));
        assert_ne!(val, CursorOptionValue::String("other".into()));
        assert_ne!(val, CursorOptionValue::Int(0));
    }

    // ── Cross-type round-trip: bits() → from_bits_truncate() ───────

    #[test]
    fn scroll_option_round_trip() {
        let original = CursorScrollOption::KEYSET_DRIVEN
            | CursorScrollOption::AUTO_FETCH
            | CursorScrollOption::CHECK_ACCEPTED_TYPES
            | CursorScrollOption::STATIC_ACCEPTABLE;
        let round_tripped = CursorScrollOption::from_bits_truncate(original.bits());
        assert_eq!(original, round_tripped);
    }

    #[test]
    fn concurrency_round_trip() {
        let original = CursorConcurrency::OPTCC
            | CursorConcurrency::CHECK_ACCEPTED_OPTS
            | CursorConcurrency::READ_ONLY_ACCEPTABLE;
        let round_tripped = CursorConcurrency::from_bits_truncate(original.bits());
        assert_eq!(original, round_tripped);
    }

    #[test]
    fn fetch_direction_round_trip() {
        let original = FetchDirection::ABSOLUTE | FetchDirection::SKIP_UPDT_CNCY;
        let round_tripped = FetchDirection::from_bits_truncate(original.bits());
        assert_eq!(original, round_tripped);
    }

    #[test]
    fn cursor_operation_round_trip() {
        let original = CursorOperation::DELETE | CursorOperation::SET_POSITION;
        let round_tripped = CursorOperation::from_bits_truncate(original.bits());
        assert_eq!(original, round_tripped);
    }

    #[test]
    fn fetch_status_round_trip() {
        let original = FetchStatus::SUCCEEDED | FetchStatus::UPDATED;
        let round_tripped = FetchStatus::from_bits_truncate(original.bits());
        assert_eq!(original, round_tripped);
    }

    // ── i32 conversion (as used when building/parsing RPC params) ──

    #[test]
    fn scroll_option_to_i32_and_back() {
        let opts = CursorScrollOption::DYNAMIC | CursorScrollOption::AUTO_FETCH;
        let wire: i32 = opts.bits() as i32;
        let restored = CursorScrollOption::from_bits_truncate(wire as u32);
        assert_eq!(opts, restored);
    }

    #[test]
    fn concurrency_to_i32_and_back() {
        let cc = CursorConcurrency::LOCKCC | CursorConcurrency::ALLOW_DIRECT;
        let wire: i32 = cc.bits() as i32;
        let restored = CursorConcurrency::from_bits_truncate(wire as u32);
        assert_eq!(cc, restored);
    }

    #[test]
    fn fetch_direction_to_i32_and_back() {
        let dir = FetchDirection::RELATIVE;
        let wire: i32 = dir.bits() as i32;
        let restored = FetchDirection::from_bits_truncate(wire as u32);
        assert_eq!(dir, restored);
    }
}
