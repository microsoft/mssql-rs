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
//! - [`CursorScrollOption`] — cursor type + modifiers + acceptability flags
//! - [`CursorConcurrency`] — locking/optimistic modes + negotiation
//! - [`FetchDirection`] — scroll direction for `sp_cursorfetch`
//! - [`CursorOperation`] — positioned UPDATE/DELETE/INSERT via `sp_cursor`
//! - [`FetchStatus`] — per-row status from `sp_cursorfetch`
//!
//! **Enums:**
//! - [`CursorStatus`] — return status from cursor RPCs
//! - [`CursorOptionCode`] — option codes for `sp_cursoroption`
//! - [`CursorOptionValue`] — typed value for `sp_cursoroption`
//!
//! **Responses:**
//! - [`CursorOpenResponse`] — from `sp_cursoropen` / `sp_cursorexecute`
//! - [`CursorPrepExecResponse`] — from `sp_cursorprepexec`
//! - [`CursorPrepareResponse`] — from `sp_cursorprepare`

mod cursor_response;
mod cursor_types;

pub use cursor_response::{CursorOpenResponse, CursorPrepExecResponse, CursorPrepareResponse};
pub use cursor_types::{
    CursorConcurrency, CursorOperation, CursorOptionCode, CursorOptionValue, CursorScrollOption,
    CursorStatus, FetchDirection, FetchStatus,
};
