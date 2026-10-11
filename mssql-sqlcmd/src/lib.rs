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

pub mod ffi;
pub mod formatter;
