// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! sqlcmd components written in Rust.
//!
//! Native sqlcmd links this crate as a static library and calls it through the
//! C ABI in [`ffi`]. Native sqlcmd still parses the command line, connects and
//! runs the batches; this crate does only the work handed to it.
//!
//! - [`formatter`] renders results in a structured format. JSON
//!   (`--format json`) is the first one.
//! - [`diagnostics`] checks a connection stage by stage for
//!   `sqlcmd diagnose`, and reports it as text or JSON. It connects with
//!   `mssql-tds`, which is 64-bit only, so 32-bit builds leave it out.

#[cfg(target_pointer_width = "64")]
pub mod diagnostics;
pub mod ffi;
pub mod formatter;
pub mod i18n;
