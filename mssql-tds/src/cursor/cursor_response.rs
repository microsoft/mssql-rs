// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Response types returned by cursor RPC methods on [`TdsClient`](crate::connection::tds_client::TdsClient).
//!
//! These structs carry the protocol-level output parameters extracted from
//! `ReturnValue` tokens. They do **not** contain row data — if the caller
//! set `AUTO_FETCH` or `RETURN_METADATA`, rows/metadata are available in the
//! token stream via `get_next_row_into()`.

use super::cursor_types::{CursorConcurrency, CursorScrollOption};

/// Response from `sp_cursoropen` (`RpcProcs::CursorOpen`),
/// `sp_cursorexecute` (`RpcProcs::CursorExecute`), or the cursor portion
/// of `sp_cursorprepexec`.
///
/// Contains the server-assigned cursor handle and the negotiated cursor
/// type/concurrency values (which may differ from what the caller requested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorOpenResponse {
    /// Server-assigned cursor handle. Pass to `cursor_fetch`, `cursor_op`,
    /// `cursor_option`, and `cursor_close`.
    pub cursor_id: i32,
    /// Cursor type the server actually granted. May differ from the
    /// requested `scrollopt` if the server downgraded.
    pub negotiated_scroll: CursorScrollOption,
    /// Concurrency the server actually granted. May differ from the
    /// requested `ccopt` if the server downgraded.
    pub negotiated_concurrency: CursorConcurrency,
    /// Row count returned by the server. When `AUTO_FETCH` is set, this
    /// is the number of rows in the first fetch batch.
    pub row_count: i32,
}

/// Response from `sp_cursorprepexec` (`RpcProcs::CursorPrepExec`).
///
/// Combines a reusable prepare handle with a cursor open response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPrepExecResponse {
    /// Prepare handle for reuse with `cursor_execute`. Also usable with
    /// `cursor_unprepare` when no longer needed.
    pub prepared_handle: i32,
    /// The cursor open portion of the response.
    pub cursor: CursorOpenResponse,
}

/// Response from `sp_cursorprepare` (`RpcProcs::CursorPrepare`).
///
/// Returns a prepare handle and the negotiated cursor type/concurrency.
/// No cursor is opened — call `cursor_execute` to open one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPrepareResponse {
    /// Prepare handle to pass to `cursor_execute` or `cursor_unprepare`.
    pub prepared_handle: i32,
    /// Cursor type the server will grant (negotiated at prepare time).
    pub negotiated_scroll: CursorScrollOption,
    /// Concurrency the server will grant (negotiated at prepare time).
    pub negotiated_concurrency: CursorConcurrency,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_open_response_construction() {
        let resp = CursorOpenResponse {
            cursor_id: 42,
            negotiated_scroll: CursorScrollOption::FORWARD_ONLY,
            negotiated_concurrency: CursorConcurrency::READONLY,
            row_count: 100,
        };
        assert_eq!(resp.cursor_id, 42);
        assert_eq!(resp.negotiated_scroll, CursorScrollOption::FORWARD_ONLY);
        assert_eq!(resp.negotiated_concurrency, CursorConcurrency::READONLY);
        assert_eq!(resp.row_count, 100);
    }

    #[test]
    fn cursor_open_response_negotiated_downgrade() {
        // Simulate server downgrading DYNAMIC → KEYSET_DRIVEN
        let resp = CursorOpenResponse {
            cursor_id: 7,
            negotiated_scroll: CursorScrollOption::KEYSET_DRIVEN,
            negotiated_concurrency: CursorConcurrency::READONLY,
            row_count: 0,
        };
        assert_ne!(resp.negotiated_scroll, CursorScrollOption::DYNAMIC);
        assert_eq!(resp.negotiated_scroll, CursorScrollOption::KEYSET_DRIVEN);
    }

    #[test]
    fn cursor_open_response_clone_and_eq() {
        let resp = CursorOpenResponse {
            cursor_id: 1,
            negotiated_scroll: CursorScrollOption::STATIC,
            negotiated_concurrency: CursorConcurrency::OPTCC,
            row_count: 50,
        };
        let cloned = resp.clone();
        assert_eq!(resp, cloned);
    }

    #[test]
    fn cursor_prepexec_response_construction() {
        let resp = CursorPrepExecResponse {
            prepared_handle: 99,
            cursor: CursorOpenResponse {
                cursor_id: 10,
                negotiated_scroll: CursorScrollOption::KEYSET_DRIVEN,
                negotiated_concurrency: CursorConcurrency::OPTCC,
                row_count: 200,
            },
        };
        assert_eq!(resp.prepared_handle, 99);
        assert_eq!(resp.cursor.cursor_id, 10);
        assert_eq!(
            resp.cursor.negotiated_scroll,
            CursorScrollOption::KEYSET_DRIVEN
        );
        assert_eq!(resp.cursor.row_count, 200);
    }

    #[test]
    fn cursor_prepexec_response_clone_and_eq() {
        let resp = CursorPrepExecResponse {
            prepared_handle: 5,
            cursor: CursorOpenResponse {
                cursor_id: 3,
                negotiated_scroll: CursorScrollOption::DYNAMIC,
                negotiated_concurrency: CursorConcurrency::LOCKCC,
                row_count: 0,
            },
        };
        let cloned = resp.clone();
        assert_eq!(resp, cloned);
    }

    #[test]
    fn cursor_prepare_response_construction() {
        let resp = CursorPrepareResponse {
            prepared_handle: 77,
            negotiated_scroll: CursorScrollOption::FORWARD_ONLY | CursorScrollOption::AUTO_CLOSE,
            negotiated_concurrency: CursorConcurrency::READONLY,
        };
        assert_eq!(resp.prepared_handle, 77);
        assert!(
            resp.negotiated_scroll
                .contains(CursorScrollOption::FORWARD_ONLY)
        );
        assert!(
            resp.negotiated_scroll
                .contains(CursorScrollOption::AUTO_CLOSE)
        );
        assert_eq!(resp.negotiated_concurrency, CursorConcurrency::READONLY);
    }

    #[test]
    fn cursor_prepare_response_clone_and_eq() {
        let resp = CursorPrepareResponse {
            prepared_handle: 12,
            negotiated_scroll: CursorScrollOption::STATIC,
            negotiated_concurrency: CursorConcurrency::OPTCCVAL,
        };
        let cloned = resp.clone();
        assert_eq!(resp, cloned);
    }
}
