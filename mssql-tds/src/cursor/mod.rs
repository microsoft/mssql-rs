// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cursor types and response structures for TDS cursor RPCs.
//!
//! This module exposes the bitflags, enums, and response types used by the
//! `sp_cursor*` family of system stored procedures (RPC IDs 1–9). These
//! types are consumed by the cursor methods on
//! [`TdsClient`](crate::connection::tds_client::TdsClient).
//!
//! # Types
//!
//! **Bitflags** (combinable via `|`):
//! - [`CursorScrollOption`](crate::cursor::CursorScrollOption) — cursor type + modifiers + acceptability flags
//! - [`CursorConcurrency`](crate::cursor::CursorConcurrency) — locking/optimistic modes + negotiation
//! - [`FetchDirection`](crate::cursor::FetchDirection) — scroll direction for `sp_cursorfetch`
//! - [`CursorOperation`](crate::cursor::CursorOperation) — positioned UPDATE/DELETE/INSERT via `sp_cursor`
//! - [`FetchStatus`](crate::cursor::FetchStatus) — per-row status from `sp_cursorfetch`
//!
//! **Enums:**
//! - [`CursorStatus`](crate::cursor::CursorStatus) — return status from cursor RPCs
//! - [`CursorOptionCode`](crate::cursor::CursorOptionCode) — option codes for `sp_cursoroption`
//! - [`CursorOptionValue`](crate::cursor::CursorOptionValue) — typed value for `sp_cursoroption`
//!
//! **Responses:**
//! - [`CursorOpenResponse`](crate::cursor::CursorOpenResponse) — from `sp_cursoropen` / `sp_cursorexecute`
//! - [`CursorPrepExecResponse`](crate::cursor::CursorPrepExecResponse) — from `sp_cursorprepexec`
//! - [`CursorPrepareResponse`](crate::cursor::CursorPrepareResponse) — from `sp_cursorprepare`

mod cursor_response;
mod cursor_types;

pub use cursor_response::{CursorOpenResponse, CursorPrepExecResponse, CursorPrepareResponse};
pub use cursor_types::{
    CursorConcurrency, CursorOperation, CursorOptionCode, CursorOptionValue, CursorScrollOption,
    CursorStatus, FetchDirection, FetchStatus,
};
