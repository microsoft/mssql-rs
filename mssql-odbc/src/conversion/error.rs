// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Direction-neutral outcome vocabulary for value conversion.
//!
//! Converters report through these types instead of posting diagnostics
//! themselves: they have no handle to post to, and the record a caller builds
//! differs by path (`SQLGetData` sets a column number; block fetch also sets a
//! row number and a row-status entry). The caller maps a variant to its
//! SQLSTATE.

/// Outcome of a successful conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvOk {
    /// The value was represented exactly.
    Exact,
    /// Precision was lost (e.g. a date/time component the target cannot hold).
    /// The caller posts `01S07` and returns `SQL_SUCCESS_WITH_INFO`.
    Truncated,
}

/// Why a value-level conversion could not be completed. The caller maps each
/// variant to the appropriate SQLSTATE on its own diagnostic target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvError {
    /// The value does not fit the requested C type (SQLSTATE `22003`).
    OutOfRange,
    /// This source/target pairing is not handled by this converter; the caller
    /// should try another path. Never surfaced to the application directly.
    NotHandledHere,
    /// The requested C type is not a legal target for this SQL type
    /// (SQLSTATE `07006`). Terminal.
    Restricted,
    /// A character column's text is not a valid literal for the requested target
    /// (SQLSTATE `22018`). Terminal.
    InvalidCharacterValue,
}
